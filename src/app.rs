//! 应用根视图：把工具栏、工作区、状态栏拼起来。
//!
//! 这一层现在很薄 —— SFTP worker、事件泵、会话状态都在 [`Workspace`] 里，
//! 这里只做两件事：
//!
//! 1. 把工具栏事件转成工作区上的方法调用；
//! 2. 把工作区抛上来的会话状态同步回工具栏（按钮文案 / 端点标签）。
//!
//! 重入提醒：GPUI 的 `Entity::update` 是独占租约，在某个实体被更新期间再更新它会
//! panic。所以工具栏事件里只更新 `workspace`，工作区事件里只更新 `toolbar`。

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    radio::{Radio, RadioGroup},
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::discovery::DiscoveryService;
use crate::session::store::{
    self, AppSettings, LanSftpPasswordMode, RemoteOpenBehavior, SavedAuth, SavedProfile,
};
use crate::sftp::lan_server::{ServerInfo, TransferActivity};
use crate::sftp::types::{AuthMethod, SessionConfig};
use crate::ui::SessionState;
use crate::ui::file_pane::FilePane;
use crate::ui::toolbar::{Toolbar, ToolbarEvent};
use crate::ui::transfer_panel::TransferQueue;
use crate::ui::workspace::{Workspace, WorkspaceEvent};
use crate::update::{self, AvailableUpdate};

pub struct ScpApp {
    toolbar: Entity<Toolbar>,
    workspace: Entity<Workspace>,
    /// 工作区里两个面板与队列的句柄，状态栏展示路径/任务数时用。
    local: Entity<FilePane>,
    remote: Entity<FilePane>,
    transfers: Entity<TransferQueue>,
    profiles: Vec<SavedProfile>,
    settings: AppSettings,
    lan_server: Result<ServerInfo, SharedString>,
    lan_transfer_activity: TransferActivity,
    lan_transfer_active: bool,
    lan_idle_sleep_guard: Option<ActivityGuard>,
    lan_idle_sleep_request_pending: bool,
    _discovery: Option<DiscoveryService>,
    /// 当前这次连接实际使用的配置。密码框可能为空（收藏会从系统凭据库补密码），
    /// 成功后必须保存这里的认证信息，不能再从空白表单反推。
    pending_config: Option<SessionConfig>,
    version: String,
    available_update: Option<AvailableUpdate>,
    _subscriptions: Vec<Subscription>,
    _discovery_pump: Task<()>,
    _lan_transfer_pump: Task<()>,
}

struct SettingsDraft {
    remote_open_behavior: RemoteOpenBehavior,
    editor_path: Option<PathBuf>,
    lan_sftp_root: Option<PathBuf>,
    lan_sftp_password_mode: LanSftpPasswordMode,
    password_visible: bool,
    error: Option<SharedString>,
}

struct ProfilePersistence {
    profile: SavedProfile,
    /// 只来自本次实际认证配置；绝不从可能为空的密码框读取。
    password: Option<String>,
}

fn prepare_profile_persistence(config: &SessionConfig) -> ProfilePersistence {
    let (auth, password) = match &config.auth {
        AuthMethod::PrivateKey { path, .. } => (
            SavedAuth::Key {
                path: path.to_string_lossy().to_string(),
            },
            None,
        ),
        AuthMethod::Password(password) => (SavedAuth::Password, Some(password.clone())),
    };
    ProfilePersistence {
        profile: SavedProfile {
            host: config.host.clone(),
            port: config.port,
            username: config.username.clone(),
            auth,
        },
        password,
    }
}

impl ScpApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let profiles = store::load_profiles();
        let settings = store::load_settings();
        let server_password = match settings.lan_sftp_password_mode {
            LanSftpPasswordMode::Default => Ok(None),
            LanSftpPasswordMode::Manual => store::load_lan_sftp_password().and_then(|password| {
                password
                    .map(Some)
                    .ok_or_else(|| "手动密码尚未设置，请在设置中填写".to_string())
            }),
        };
        let (lan_transfer_activity, lan_transfer_events) =
            crate::sftp::lan_server::transfer_activity();
        let lan_server = server_password
            .and_then(|password| {
                crate::sftp::lan_server::start(
                    settings.lan_sftp_root.clone(),
                    password,
                    lan_transfer_activity.clone(),
                )
            })
            .map_err(|message| {
                log::error!("内置局域网 SFTP 服务启动失败：{message}");
                SharedString::from(message)
            });
        if let Ok(info) = &lan_server {
            log::info!(
                "局域网 SFTP 已启动：{}@{}，共享目录 {}",
                info.username,
                info.endpoint(),
                info.root().display()
            );
        }
        let toolbar = cx.new(|cx| Toolbar::new(profiles.clone(), window, cx));
        let discovery =
            match crate::discovery::start(lan_server.as_ref().ok().map(|info| info.port)) {
                Ok(discovery) => Some(discovery),
                Err(message) => {
                    log::error!("局域网设备发现启动失败：{message}");
                    None
                }
            };
        let discovery_events = discovery
            .as_ref()
            .map(|discovery| discovery.events().clone());
        let discovery_pump = cx.spawn(async move |this, cx| {
            let Some(events) = discovery_events else {
                return;
            };
            while let Ok(devices) = events.recv_async().await {
                if this
                    .update(cx, |app, cx| {
                        app.toolbar.update(cx, |toolbar, cx| {
                            toolbar.set_discovered_devices(devices, cx)
                        });
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let lan_transfer_pump = cx.spawn(async move |this, cx| {
            while let Ok(active) = lan_transfer_events.recv_async().await {
                if this
                    .update(cx, |app, cx| app.set_lan_transfer_active(active, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        // 工作区在构造时就启动 SftpWorker 与事件泵。
        let workspace = cx.new(|cx| Workspace::new(settings.clone(), window, cx));

        let (local, remote, transfers) = {
            let ws = workspace.read(cx);
            (ws.local(), ws.remote(), ws.transfers())
        };

        let mut subscriptions = Vec::new();

        // 工具栏 → 工作区 / 根实体。不回头动 toolbar（它正被更新）。
        //
        // 标题栏也在这里改：手动「断开」不会让工作区发 `SessionStateChanged`，
        // 不在这儿兜一下，标题会一直挂着上一个连接。
        // 注意只能改窗口，**不能读 toolbar** —— 此刻它正被更新。
        subscriptions.push(cx.subscribe_in(
            &toolbar,
            window,
            |app, _toolbar, event: &ToolbarEvent, window, cx| match event {
                ToolbarEvent::Connect(config) => {
                    window.set_window_title(&format!(
                        "Blip — 正在连接 {}@{}…",
                        config.username, config.host
                    ));
                    let config = config.clone();
                    app.pending_config = Some(config.clone());
                    app.workspace
                        .update(cx, |workspace, cx| workspace.connect(config, cx));
                }
                ToolbarEvent::Disconnect => {
                    app.pending_config = None;
                    app.workspace
                        .update(cx, |workspace, cx| workspace.disconnect(cx));
                    window.set_window_title("Blip");
                }
                ToolbarEvent::InstallPublicKey {
                    public_key,
                    private_key,
                } => {
                    app.workspace.update(cx, |workspace, cx| {
                        workspace.install_public_key(public_key.clone(), private_key.clone(), cx)
                    });
                }
                ToolbarEvent::ProfilesChanged(profiles) => {
                    app.profiles = profiles.clone();
                }
                ToolbarEvent::OpenSettings => app.open_settings(window, cx),
            },
        ));

        // 工作区 → 工具栏：只同步由事件泵推动的状态变化。
        subscriptions.push(cx.subscribe_in(
            &workspace,
            window,
            |app, _workspace, event: &WorkspaceEvent, window, cx| {
                match event {
                    WorkspaceEvent::SessionStateChanged(state) => {
                        let state = *state;
                        let was_connected = app.toolbar.read(cx).state() == SessionState::Connected;
                        // 连接失败的原因（认证被拒、私钥口令错、超时…）要出现在表单旁边，
                        // 只塞进底部状态栏等于没告诉用户。
                        let last_error = app.workspace.read(cx).last_error();

                        app.toolbar.update(cx, |toolbar, cx| {
                            toolbar.set_state(state, cx);
                            if state == SessionState::Connected {
                                toolbar.set_error(None, cx);
                            } else if let Some(error) = last_error {
                                toolbar.set_error(Some(error), cx);
                            }
                        });

                        // 标题栏跟着会话状态走：`Blip — user@host`。
                        app.sync_window_title(window, cx);

                        // 连接成功才落盘凭证，避免把错误密码写进钥匙串。
                        if state == SessionState::Connected && !was_connected {
                            app.persist_profile(cx);
                        } else if state == SessionState::Disconnected {
                            // 认证失败、超时或主动断开后，绝不能把这份配置用于下一次成功事件。
                            app.pending_config = None;
                        }
                    }
                    WorkspaceEvent::TransferPoolChanged => cx.notify(),
                    WorkspaceEvent::PublicKeyInstalled { private_key } => {
                        app.toolbar.update(cx, |toolbar, cx| {
                            toolbar.finish_public_key_install(Ok(private_key.clone()), cx)
                        });
                        app.persist_installed_key(private_key, cx);
                    }
                    WorkspaceEvent::PublicKeyInstallFailed(message) => {
                        app.toolbar.update(cx, |toolbar, cx| {
                            toolbar.finish_public_key_install(Err(message.clone()), cx)
                        });
                    }
                }
            },
        ));

        let mut app = Self {
            toolbar,
            workspace,
            local,
            remote,
            transfers,
            profiles,
            settings,
            lan_server,
            lan_transfer_activity,
            lan_transfer_active: false,
            lan_idle_sleep_guard: None,
            lan_idle_sleep_request_pending: false,
            _discovery: discovery,
            pending_config: None,
            version: update::application_version(),
            available_update: None,
            _subscriptions: subscriptions,
            _discovery_pump: discovery_pump,
            _lan_transfer_pump: lan_transfer_pump,
        };

        app.prefill_last_profile(window, cx);
        app.sync_window_title(window, cx);
        app.check_for_updates(cx);
        app
    }

    fn set_lan_transfer_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.lan_transfer_active = active;
        if !active {
            self.lan_idle_sleep_guard = None;
            return;
        }
        if self.lan_idle_sleep_guard.is_some() || self.lan_idle_sleep_request_pending {
            return;
        }

        self.lan_idle_sleep_request_pending = true;
        let request = cx.prevent_idle_sleep("Blip 局域网 SFTP 正在传输文件");
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |app, _cx| {
                app.lan_idle_sleep_request_pending = false;
                match result {
                    Ok(guard) if app.lan_transfer_active => app.lan_idle_sleep_guard = Some(guard),
                    Ok(_) => {}
                    Err(err) => log::warn!("无法阻止局域网传输期间系统休眠：{err:#}"),
                }
            });
        })
        .detach();
    }

    fn check_for_updates(&self, cx: &mut Context<Self>) {
        let current_version = self.version.clone();
        let http_client = cx.http_client();
        cx.spawn(async move |this, cx| {
            let result = update::check_latest_release(http_client, &current_version).await;
            match result {
                Ok(Some(available)) => {
                    let _ = this.update_in(cx, |app, window, cx| {
                        app.available_update = Some(available);
                        if !window.has_active_dialog(cx) {
                            app.open_update_dialog(window, cx);
                        }
                        cx.notify();
                    });
                }
                Ok(None) => {}
                Err(error) => log::debug!("自动检查更新失败：{error}"),
            }
        })
        .detach();
    }

    fn open_update_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(available) = self.available_update.clone() else {
            return;
        };
        let release_url = available.release_url.clone();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let release_url = release_url.clone();
            dialog
                .title("发现新版本")
                .w(px(440.))
                .overlay_closable(false)
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().child(SharedString::from(format!(
                            "Blip {} 已发布，当前版本为 {}。",
                            available.version, available.current_version
                        ))))
                        .child(
                            div()
                                .text_xs()
                                .text_color(_cx.theme().muted_foreground)
                                .child("将打开 GitHub Releases，请下载适合当前系统的安装包。"),
                        ),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("update-later")
                                .label("稍后")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("update-download")
                                .primary()
                                .label("下载升级")
                                .on_click(move |_, window, cx| {
                                    cx.open_url(&release_url);
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn open_lan_server_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let server = self.lan_server.clone();
        let manual_password = self.settings.lan_sftp_password_mode == LanSftpPasswordMode::Manual;
        window.open_dialog(cx, move |dialog, _window, cx| match server.clone() {
            Ok(info) => {
                let theme = cx.theme().clone();
                let command = info.command();
                let password = info.password();
                let copy_command = command.clone();
                let copy_password = password.clone();

                dialog
                    .title("局域网 SFTP")
                    .w(px(560.))
                    .overlay_closable(false)
                    .child(
                        v_flex()
                            .w_full()
                            .gap_3()
                            .child(
                                div()
                                    .text_sm()
                                    .child("同一局域网内的设备可使用以下信息连接到这台电脑。"),
                            )
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .w(px(72.))
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child("连接命令"),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .px_3()
                                                    .py_2()
                                                    .rounded_md()
                                                    .border_1()
                                                    .border_color(theme.border)
                                                    .text_sm()
                                                    .truncate()
                                                    .child(SharedString::from(command)),
                                            )
                                            .child(
                                                Button::new("copy-lan-sftp-command")
                                                    .label("复制")
                                                    .on_click(move |_, window, cx| {
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(
                                                                copy_command.clone(),
                                                            ),
                                                        );
                                                        window.push_notification(
                                                            Notification::success("连接命令已复制"),
                                                            cx,
                                                        );
                                                    }),
                                            ),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .w(px(72.))
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child("登录密码"),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .px_3()
                                                    .py_2()
                                                    .rounded_md()
                                                    .border_1()
                                                    .border_color(theme.border)
                                                    .text_sm()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child(SharedString::from(password)),
                                            )
                                            .child(
                                                Button::new("copy-lan-sftp-password")
                                                    .label("复制")
                                                    .on_click(move |_, window, cx| {
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(
                                                                copy_password.clone(),
                                                            ),
                                                        );
                                                        window.push_notification(
                                                            Notification::success("SFTP 密码已复制"),
                                                            cx,
                                                        );
                                                    }),
                                            ),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .w(px(72.))
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child("共享目录"),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_sm()
                                                    .truncate()
                                                    .child(SharedString::from(
                                                        display_local_path(&info.root()),
                                                    )),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(if manual_password {
                                        "当前使用手动密码。请只在可信局域网中使用；退出 Blip 后服务自动停止。"
                                    } else {
                                        "当前使用默认密码，可在设置中改为手动密码。请只在可信局域网中使用；退出 Blip 后服务自动停止。"
                                    }),
                            ),
                    )
                    .footer(
                        h_flex().justify_end().child(
                            Button::new("close-lan-sftp")
                                .primary()
                                .label("完成")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                    )
            }
            Err(message) => dialog
                .title("局域网 SFTP 启动失败")
                .w(px(480.))
                .overlay_closable(false)
                .child(v_flex().gap_2().child(message).child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("请在设置中重新选择共享目录；若端口或网络权限导致失败，解决后保存设置即可重试。"),
                ))
                .footer(
                    h_flex().justify_end().child(
                        Button::new("close-lan-sftp-error")
                            .primary()
                            .label("关闭")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                ),
        });
    }

    // -----------------------------------------------------------------------
    // 凭证
    // -----------------------------------------------------------------------

    /// 把当前连接写进系统凭据库 + profiles.json。
    ///
    /// 用户关掉「记住」时**什么都不落盘**：系统凭据库里的旧密码顺手清掉，
    /// profiles.json 也不写 —— 否则演示模式（remember = false）会往
    /// 配置里塞一堆 `tester@127.0.0.1:<随机端口>`，下次启动还会被
    /// `prefill` 填回表单。
    fn persist_profile(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.pending_config.clone() else {
            log::warn!("连接成功但缺少实际连接配置，跳过收藏持久化");
            return;
        };
        let snapshot = self.toolbar.read(cx).snapshot(cx);
        let prepared = prepare_profile_persistence(&config);

        if !snapshot.remember {
            if matches!(config.auth, AuthMethod::Password(_)) {
                store::forget_password(&config);
            }
            return;
        }

        if let Some(password) = prepared.password {
            let password_config = config.clone();
            // 写系统凭据库可能触发授权弹窗，放到后台线程；失败时回到前台
            // 把原因显示在工具栏上 —— 只写日志等于没告诉用户。
            let this = cx.entity().downgrade();
            cx.spawn(async move |_app, cx| {
                let task = cx
                    .background_executor()
                    .spawn(async move { store::store_password(&password_config, &password) });

                if let Err(err) = task.await {
                    log::warn!("保存密码失败：{err}");
                    let _ = this.update(cx, |app, cx| {
                        let message =
                            SharedString::from(format!("保存密码到系统凭据库失败：{err}"));
                        app.toolbar
                            .update(cx, |toolbar, cx| toolbar.set_error(Some(message), cx));
                    });
                }
            })
            .detach();
        }

        let profile = prepared.profile;
        store::upsert_profile(&mut self.profiles, profile);
        let profiles = self.profiles.clone();
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.set_profiles(profiles, cx));
        if let Err(err) = store::save_profiles(&self.profiles) {
            log::warn!("保存连接配置失败：{err}");
        }
    }

    fn persist_installed_key(&mut self, private_key: &std::path::Path, cx: &mut Context<Self>) {
        let Some(config) = self.pending_config.as_ref() else {
            self.toolbar.update(cx, |toolbar, cx| {
                toolbar.set_error(Some("公钥已部署，但无法更新本地连接配置".into()), cx)
            });
            return;
        };
        if !self.toolbar.read(cx).snapshot(cx).remember {
            return;
        }

        store::upsert_profile(
            &mut self.profiles,
            SavedProfile {
                host: config.host.clone(),
                port: config.port,
                username: config.username.clone(),
                auth: SavedAuth::Key {
                    path: private_key.to_string_lossy().to_string(),
                },
            },
        );
        let profiles = self.profiles.clone();
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.set_profiles(profiles, cx));
        if let Err(err) = store::save_profiles(&self.profiles) {
            log::warn!("保存密钥连接配置失败：{err}");
            self.toolbar.update(cx, |toolbar, cx| {
                toolbar.set_error(
                    Some(SharedString::from(format!(
                        "公钥已部署，但保存连接配置失败：{err}"
                    ))),
                    cx,
                )
            });
        }
    }

    /// 启动时把最近一次成功的连接填回表单。
    fn prefill_last_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.first().cloned() else {
            return;
        };
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.prefill(&profile, window, cx));
    }

    /// 把连接状态写进窗口标题栏：`Blip — root@192.168.146.7`。
    ///
    /// 这串 `user@host` 原来贴在表单行的最右边，和输入框挤在一起，
    /// 长得就像一个输入框；挪到窗口顶部后，表单那一行也宽松了。
    /// 断开时退回纯 `Blip`。
    fn sync_window_title(&self, window: &mut Window, cx: &App) {
        let toolbar = self.toolbar.read(cx);
        let title = match toolbar.state() {
            SessionState::Connected => {
                let snapshot = toolbar.snapshot(cx);
                format!("Blip — {}@{}", snapshot.username, snapshot.host)
            }
            SessionState::Connecting => "Blip — 正在连接…".to_string(),
            SessionState::Disconnected => "Blip".to_string(),
        };
        window.set_window_title(&title);
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password_mode = self.settings.lan_sftp_password_mode;
        let (manual_password, password_error) = match store::load_lan_sftp_password() {
            Ok(password) => (password.unwrap_or_default(), None),
            Err(message) => (String::new(), Some(SharedString::from(message))),
        };
        let manual_password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(manual_password)
                .masked(true)
                .placeholder("输入手动密码")
        });
        let draft = cx.new(|_| SettingsDraft {
            remote_open_behavior: self.settings.remote_open_behavior,
            editor_path: self.settings.editor_path.clone(),
            lan_sftp_root: self.settings.lan_sftp_root.clone(),
            lan_sftp_password_mode: password_mode,
            password_visible: false,
            error: if password_mode == LanSftpPasswordMode::Manual {
                password_error
            } else {
                None
            },
        });
        let app = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _window, cx| {
            let theme = cx.theme().clone();
            let (
                selected,
                editor_path,
                lan_sftp_root,
                lan_sftp_password_mode,
                password_visible,
                error,
            ) = {
                let snapshot = draft.read(cx);
                (
                    match snapshot.remote_open_behavior {
                        RemoteOpenBehavior::SystemDefault => 0,
                        RemoteOpenBehavior::CustomEditor => 1,
                        RemoteOpenBehavior::BuiltInTextEditor => 2,
                    },
                    snapshot.editor_path.clone(),
                    snapshot.lan_sftp_root.clone(),
                    snapshot.lan_sftp_password_mode,
                    snapshot.password_visible,
                    snapshot.error.clone(),
                )
            };

            let mode_draft = draft.clone();
            let browse_draft = draft.clone();
            let browse_share_draft = draft.clone();
            let default_share_draft = draft.clone();
            let password_mode_draft = draft.clone();
            let password_visibility_draft = draft.clone();
            let password_visibility_input = manual_password_input.clone();
            let save_draft = draft.clone();
            let save_password_input = manual_password_input.clone();
            let save_app = app.clone();

            dialog
                .title("设置")
                .w(px(600.))
                .close_button(false)
                .overlay_closable(false)
                .child(
                    v_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            v_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("默认编辑器"),
                                )
                                .child(
                                    RadioGroup::vertical("remote-open-behavior")
                                        .selected_index(Some(selected))
                                        .on_click(move |index, _, cx| {
                                            mode_draft.update(cx, |draft, cx| {
                                                draft.remote_open_behavior = match *index {
                                                    0 => RemoteOpenBehavior::SystemDefault,
                                                    1 => RemoteOpenBehavior::CustomEditor,
                                                    _ => RemoteOpenBehavior::BuiltInTextEditor,
                                                };
                                                draft.error = None;
                                                cx.notify();
                                            });
                                        })
                                        .child(
                                            Radio::new("system-editor")
                                                .label("系统默认应用"),
                                        )
                                        .child(
                                            Radio::new("custom-editor")
                                                .label("指定编辑器（忽略文件类型）"),
                                        )
                                        .child(
                                            Radio::new("built-in-editor")
                                                .label("内置文本编辑器"),
                                        ),
                                )
                                .when(
                                    selected == 1,
                                    |this| {
                                        let path_text = editor_path
                                            .as_ref()
                                            .map(|path| path.to_string_lossy().to_string())
                                            .unwrap_or_else(|| "尚未选择编辑器".to_string());
                                        this.child(
                                            h_flex()
                                                .w_full()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .px_3()
                                                        .py_2()
                                                        .rounded_md()
                                                        .border_1()
                                                        .border_color(theme.border)
                                                        .text_sm()
                                                        .text_ellipsis()
                                                        .overflow_hidden()
                                                        .child(SharedString::from(path_text)),
                                                )
                                                .child(
                                                    Button::new("choose-editor")
                                                        .label("选择应用或命令…")
                                                        .on_click(move |_, _, cx| {
                                                            let receiver = cx.prompt_for_paths(
                                                                PathPromptOptions {
                                                                    files: true,
                                                                    directories: cfg!(target_os = "macos"),
                                                                    multiple: false,
                                                                    prompt: Some(
                                                                        "选择默认编辑器".into(),
                                                                    ),
                                                                },
                                                            );
                                                            let draft = browse_draft.clone();
                                                            cx.spawn(async move |cx| {
                                                                match receiver.await {
                                                                    Ok(Ok(Some(paths))) => {
                                                                        if let Some(path) =
                                                                            paths.into_iter().next()
                                                                        {
                                                                            draft.update(
                                                                                cx,
                                                                                |draft, cx| {
                                                                                    draft.error = (!is_editor_path(&path)).then(|| {
                                                                                        SharedString::from("请选择应用或可执行文件")
                                                                                    });
                                                                                    draft.editor_path = Some(path);
                                                                                    cx.notify();
                                                                                },
                                                                            );
                                                                        }
                                                                    }
                                                                    Ok(Err(err)) => {
                                                                        draft.update(
                                                                            cx,
                                                                            |draft, cx| {
                                                                                draft.error = Some(
                                                                                    SharedString::from(
                                                                                        format!(
                                                                                            "无法选择编辑器：{err}"
                                                                                        ),
                                                                                    ),
                                                                                );
                                                                                cx.notify();
                                                                            },
                                                                        );
                                                                    }
                                                                    _ => {}
                                                                }
                                                            })
                                                            .detach();
                                                        }),
                                                ),
                                        )
                                    },
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(
                                            "外部编辑器会先把文件放入编辑缓存，打开后监测保存并自动同步到远程；内置编辑器单文件上限 2 MiB。",
                                        ),
                                ),
                        )
                        .child(
                            v_flex()
                                .gap_2()
                                .pt_3()
                                .border_t_1()
                                .border_color(theme.border)
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("局域网 SFTP"),
                                )
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child("登录密码"),
                                        )
                                        .child(
                                            RadioGroup::vertical("lan-sftp-password-mode")
                                                .selected_index(Some(
                                                    if lan_sftp_password_mode
                                                        == LanSftpPasswordMode::Default
                                                    {
                                                        0
                                                    } else {
                                                        1
                                                    },
                                                ))
                                                .on_click(move |index, _, cx| {
                                                    password_mode_draft.update(
                                                        cx,
                                                        |draft, cx| {
                                                            draft.lan_sftp_password_mode =
                                                                if *index == 0 {
                                                                    LanSftpPasswordMode::Default
                                                                } else {
                                                                    LanSftpPasswordMode::Manual
                                                                };
                                                            draft.error = None;
                                                            cx.notify();
                                                        },
                                                    );
                                                })
                                                .child(
                                                    Radio::new("lan-sftp-password-default")
                                                        .label("使用默认密码 88888888"),
                                                )
                                                .child(
                                                    Radio::new("lan-sftp-password-manual")
                                                        .label("使用手动密码"),
                                                ),
                                        )
                                        .when(
                                            lan_sftp_password_mode
                                                == LanSftpPasswordMode::Manual,
                                            |this| {
                                                this.child(
                                                    h_flex()
                                                        .w_full()
                                                        .gap_2()
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w_0()
                                                                .child(
                                                                    Input::new(
                                                                        &manual_password_input,
                                                                    )
                                                                    .id(
                                                                        "lan-sftp-manual-password",
                                                                    )
                                                                    .w_full(),
                                                                ),
                                                        )
                                                        .child(
                                                            Button::new(
                                                                "toggle-lan-sftp-password",
                                                            )
                                                            .label(if password_visible {
                                                                "隐藏"
                                                            } else {
                                                                "显示"
                                                            })
                                                            .on_click(move |_, window, cx| {
                                                                let visible =
                                                                    !password_visibility_draft
                                                                        .read(cx)
                                                                        .password_visible;
                                                                password_visibility_input.update(
                                                                    cx,
                                                                    |input, cx| {
                                                                        input.set_masked(
                                                                            !visible, window, cx,
                                                                        )
                                                                    },
                                                                );
                                                                password_visibility_draft.update(
                                                                    cx,
                                                                    |draft, cx| {
                                                                        draft.password_visible =
                                                                            visible;
                                                                        cx.notify();
                                                                    },
                                                                );
                                                            }),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .child("手动密码保存在系统凭据库中，不写入设置文件。"),
                                                )
                                            },
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("共享目录"),
                                )
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .px_3()
                                                .py_2()
                                                .rounded_md()
                                                .border_1()
                                                .border_color(theme.border)
                                                .text_sm()
                                                .truncate()
                                                .child(SharedString::from(
                                                    lan_sftp_root
                                                        .clone()
                                                        .or_else(dirs::home_dir)
                                                        .map(|path| display_local_path(&path))
                                                        .unwrap_or_else(|| {
                                                            "无法确定用户主目录".to_string()
                                                        }),
                                                )),
                                        )
                                        .child(
                                            Button::new("choose-lan-sftp-root")
                                                .label("选择文件夹…")
                                                .on_click(move |_, _, cx| {
                                                    let receiver = cx.prompt_for_paths(
                                                        PathPromptOptions {
                                                            files: false,
                                                            directories: true,
                                                            multiple: false,
                                                            prompt: Some(
                                                                "选择局域网 SFTP 共享目录".into(),
                                                            ),
                                                        },
                                                    );
                                                    let draft = browse_share_draft.clone();
                                                    cx.spawn(async move |cx| {
                                                        match receiver.await {
                                                            Ok(Ok(Some(paths))) => {
                                                                if let Some(path) =
                                                                    paths.into_iter().next()
                                                                {
                                                                    draft.update(
                                                                        cx,
                                                                        |draft, cx| {
                                                                            draft.lan_sftp_root =
                                                                                Some(path);
                                                                            draft.error = None;
                                                                            cx.notify();
                                                                        },
                                                                    );
                                                                }
                                                            }
                                                            Ok(Err(err)) => {
                                                                draft.update(
                                                                    cx,
                                                                    |draft, cx| {
                                                                        draft.error = Some(
                                                                            SharedString::from(
                                                                                format!(
                                                                                    "无法选择共享目录：{err}"
                                                                                ),
                                                                            ),
                                                                        );
                                                                        cx.notify();
                                                                    },
                                                                );
                                                            }
                                                            _ => {}
                                                        }
                                                    })
                                                    .detach();
                                                }),
                                        )
                                        .when(lan_sftp_root.is_some(), |this| {
                                            this.child(
                                                Button::new("reset-lan-sftp-root")
                                                    .label("恢复默认")
                                                    .on_click(move |_, _, cx| {
                                                        default_share_draft.update(
                                                            cx,
                                                            |draft, cx| {
                                                                draft.lan_sftp_root = None;
                                                                draft.error = None;
                                                                cx.notify();
                                                            },
                                                        );
                                                    }),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("默认共享当前用户主目录；更改后立即应用到新的 SFTP 文件操作。"),
                                ),
                        )
                        .when_some(error, |this, error| {
                            this.child(div().text_xs().text_color(theme.danger).child(error))
                        }),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("settings-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("settings-save")
                                .primary()
                                .label("保存")
                                .on_click(move |_, window, cx| {
                                    let (
                                        behavior,
                                        editor_path,
                                        lan_sftp_root,
                                        lan_sftp_password_mode,
                                    ) = {
                                        let snapshot = save_draft.read(cx);
                                        (
                                            snapshot.remote_open_behavior,
                                            snapshot.editor_path.clone(),
                                            snapshot.lan_sftp_root.clone(),
                                            snapshot.lan_sftp_password_mode,
                                        )
                                    };
                                    if behavior == RemoteOpenBehavior::CustomEditor
                                        && editor_path.as_ref().is_none_or(|path| {
                                            !is_editor_path(path)
                                        })
                                    {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some(SharedString::from(
                                                "请选择编辑器应用或可执行文件",
                                            ));
                                            cx.notify();
                                        });
                                        return;
                                    }
                                    let Some(effective_root) = lan_sftp_root
                                        .clone()
                                        .or_else(dirs::home_dir)
                                    else {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error =
                                                Some("无法确定用户主目录，请手动选择共享目录".into());
                                            cx.notify();
                                        });
                                        return;
                                    };
                                    let Ok(effective_root) = effective_root.canonicalize() else {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some("共享目录不存在或无法访问".into());
                                            cx.notify();
                                        });
                                        return;
                                    };
                                    if !effective_root.is_dir() {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some("请选择一个文件夹作为共享目录".into());
                                            cx.notify();
                                        });
                                        return;
                                    }
                                    let manual_password =
                                        if lan_sftp_password_mode == LanSftpPasswordMode::Manual {
                                            let password = save_password_input
                                                .read(cx)
                                                .value()
                                                .to_string();
                                            if password.is_empty() {
                                                save_draft.update(cx, |draft, cx| {
                                                    draft.error =
                                                        Some("手动密码不能为空".into());
                                                    cx.notify();
                                                });
                                                return;
                                            }
                                            if let Err(message) =
                                                store::store_lan_sftp_password(&password)
                                            {
                                                save_draft.update(cx, |draft, cx| {
                                                    draft.error =
                                                        Some(SharedString::from(message));
                                                    cx.notify();
                                                });
                                                return;
                                            }
                                            Some(password)
                                        } else {
                                            None
                                        };
                                    let settings = AppSettings {
                                        remote_open_behavior: behavior,
                                        editor_path,
                                        lan_sftp_root,
                                        lan_sftp_password_mode,
                                    };
                                    if let Err(message) = store::save_settings(&settings) {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some(SharedString::from(message));
                                            cx.notify();
                                        });
                                        return;
                                    }

                                    let update_result = save_app.update(cx, |app, cx| {
                                        app.lan_server = match &app.lan_server {
                                            Ok(server) => server
                                                .set_root(effective_root.clone())
                                                .and_then(|_| match lan_sftp_password_mode {
                                                    LanSftpPasswordMode::Manual => server
                                                        .set_password(manual_password.clone()),
                                                    LanSftpPasswordMode::Default
                                                        if app.settings.lan_sftp_password_mode
                                                            == LanSftpPasswordMode::Manual =>
                                                    {
                                                        server.set_password(None)
                                                    }
                                                    LanSftpPasswordMode::Default => Ok(()),
                                                })
                                                .map(|_| server.clone())
                                                .map_err(SharedString::from),
                                            Err(_) => crate::sftp::lan_server::start(
                                                settings.lan_sftp_root.clone(),
                                                manual_password.clone(),
                                                app.lan_transfer_activity.clone(),
                                            )
                                            .map_err(SharedString::from),
                                        };
                                        app.settings = settings.clone();
                                        if let Some(discovery) = &app._discovery {
                                            let port = app
                                                .lan_server
                                                .as_ref()
                                                .ok()
                                                .map(|server| server.port);
                                            if let Err(message) = discovery.set_port(port) {
                                                log::warn!(
                                                    "更新设备发现服务端口失败：{message}"
                                                );
                                            }
                                        }
                                        app.workspace.update(cx, |workspace, cx| {
                                            workspace.set_settings(settings.clone(), cx)
                                        });
                                        app.lan_server.as_ref().err().cloned()
                                    });
                                    window.close_dialog(cx);
                                    if let Ok(Some(message)) = update_result {
                                        window.push_notification(
                                            Notification::error(format!(
                                                "设置已保存，但局域网 SFTP 启动失败：{message}"
                                            )),
                                            cx,
                                        );
                                    } else {
                                        window.push_notification(
                                            Notification::success("设置已保存"),
                                            cx,
                                        );
                                    }
                                }),
                        ),
                )
        });
    }

    /// 演示模式：把内置服务器的地址填进表单并直接连上。
    ///
    /// 只在 `--features demo-server` 下编译。故意 `remember = false` ——
    /// 别把假账号写进用户的系统凭据库。
    #[cfg(feature = "demo-server")]
    pub fn apply_demo_login(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(login) = crate::demo::login() else {
            return;
        };

        self.toolbar.update(cx, |toolbar, cx| {
            toolbar.fill_credentials(
                &login.host,
                login.port,
                &login.username,
                &login.password,
                false,
                window,
                cx,
            );
        });

        let config = crate::sftp::types::SessionConfig {
            host: login.host.clone(),
            port: login.port,
            username: login.username.clone(),
            auth: AuthMethod::Password(login.password.clone()),
        };
        self.workspace
            .update(cx, |workspace, cx| workspace.connect(config, cx));
    }

    // -----------------------------------------------------------------------
    // 渲染
    // -----------------------------------------------------------------------

    fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let local_path = self.local.read(cx).path().to_string();
        let remote_path = self.remote.read(cx).path().to_string();
        let active = self.transfers.read(cx).active_count();
        let (last_error, upload_sources, upload_concurrency) = {
            let workspace = self.workspace.read(cx);
            let (upload_sources, upload_concurrency) = workspace.upload_pool_status();
            (workspace.last_error(), upload_sources, upload_concurrency)
        };

        h_flex()
            .w_full()
            .flex_shrink_0()
            .justify_between()
            .gap_3()
            .px_4()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(format!("本地 · {local_path}"))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(format!("远端 · {remote_path}"))),
            )
            .when_some(last_error, |this, error| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .gap_1()
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .when(!upload_sources.is_empty(), |this| {
                let upload_links = upload_sources.len();
                let tooltip = format!(
                    "上传连接：\n{}\n\n当前总并发上限：{upload_concurrency} 路",
                    upload_sources.join("\n")
                );
                this.child(
                    div()
                        .id("upload-pool-status")
                        .flex_shrink_0()
                        .text_color(if upload_links > 1 {
                            theme.success
                        } else {
                            theme.muted_foreground
                        })
                        .child(SharedString::from(format!(
                            "上传 {upload_links} 链路 · {upload_concurrency} 路"
                        )))
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx)),
                )
            })
            .child(
                div()
                    .flex_shrink_0()
                    .child(SharedString::from(if active > 0 {
                        format!("{active} 个传输进行中")
                    } else {
                        "空闲".to_string()
                    })),
            )
            .child(match &self.lan_server {
                Ok(info) => Button::new("lan-sftp-status")
                    .link()
                    .xsmall()
                    .compact()
                    .label(SharedString::from(format!("SFTP · {}", info.endpoint())))
                    .tooltip("查看局域网 SFTP 登录信息")
                    .on_click(
                        cx.listener(|app, _, window, cx| app.open_lan_server_dialog(window, cx)),
                    )
                    .into_any_element(),
                Err(_) => Button::new("lan-sftp-status")
                    .link()
                    .xsmall()
                    .compact()
                    .label("SFTP · 启动失败")
                    .tooltip("查看局域网 SFTP 错误")
                    .on_click(
                        cx.listener(|app, _, window, cx| app.open_lan_server_dialog(window, cx)),
                    )
                    .into_any_element(),
            })
            .child(if self.available_update.is_some() {
                Button::new("app-version")
                    .link()
                    .xsmall()
                    .compact()
                    .label(SharedString::from(format!("v{} · 可升级", self.version)))
                    .tooltip("打开版本更新")
                    .on_click(cx.listener(|app, _, window, cx| app.open_update_dialog(window, cx)))
                    .into_any_element()
            } else {
                div()
                    .flex_shrink_0()
                    .child(SharedString::from(format!("v{}", self.version)))
                    .into_any_element()
            })
            .into_any_element()
    }
}

impl Render for ScpApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let status = self.render_status_bar(cx);

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.toolbar.clone())
            .child(div().flex_1().min_h_0().child(self.workspace.clone()))
            .child(status)
    }
}

fn is_editor_path(path: &std::path::Path) -> bool {
    path.is_file()
        || (cfg!(target_os = "macos")
            && path.is_dir()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app")))
}

fn display_local_path(path: &std::path::Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(target_os = "windows") {
        return strip_windows_verbatim_prefix(&path);
    }
    path.into_owned()
}

fn strip_windows_verbatim_prefix(path: &str) -> String {
    if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{path}");
    }
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

/// 把 `~/xxx` 展开成真实路径；其它输入原样返回。
///
/// 留空时返回空 `PathBuf`，交给 worker 去 `~/.ssh` 里自动找私钥。
#[allow(dead_code)]
fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(input)
}

#[cfg(test)]
mod tests {
    use ::core::prelude::v1::test;

    use super::*;

    #[test]
    fn favorite_persists_the_password_that_was_actually_used() {
        let config = SessionConfig {
            host: "server.example".to_string(),
            port: 2222,
            username: "alice".to_string(),
            // 这可以是 build_config 从系统凭据库补出来的密码；密码输入框仍可能为空。
            auth: AuthMethod::Password("resolved-from-keychain".to_string()),
        };

        let prepared = prepare_profile_persistence(&config);

        assert_eq!(prepared.password.as_deref(), Some("resolved-from-keychain"));
        assert_eq!(prepared.profile.host, "server.example");
        assert_eq!(prepared.profile.port, 2222);
        assert_eq!(prepared.profile.username, "alice");
        assert!(matches!(prepared.profile.auth, SavedAuth::Password));
    }

    #[test]
    fn favorite_never_persists_a_private_key_passphrase() {
        let config = SessionConfig {
            host: "server.example".to_string(),
            port: 22,
            username: "root".to_string(),
            auth: AuthMethod::PrivateKey {
                path: PathBuf::from("/tmp/id_ed25519"),
                passphrase: Some("do-not-save".to_string()),
            },
        };

        let prepared = prepare_profile_persistence(&config);

        assert_eq!(prepared.password, None);
        assert!(matches!(
            prepared.profile.auth,
            SavedAuth::Key { ref path } if path == "/tmp/id_ed25519"
        ));
    }

    #[test]
    fn windows_verbatim_paths_are_human_readable() {
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\C:\Users\zy\Shared"),
            r"C:\Users\zy\Shared"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\UNC\server\Shared"),
            r"\\server\Shared"
        );
    }
}
