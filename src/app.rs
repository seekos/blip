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

use std::{cell::Cell, io::Cursor, path::PathBuf, rc::Rc, sync::Arc};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, WindowExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    radio::{Radio, RadioGroup},
    scroll::ScrollableElement as _,
    tab::{Tab, TabBar},
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use image::{DynamicImage, ImageFormat as RasterImageFormat, Luma};
use qrcode::QrCode;

use crate::discovery::DiscoveryService;
use crate::http_share::HttpServer;
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

fn qr_code_image(value: &str) -> Result<(Arc<Image>, f32), String> {
    let code = QrCode::new(value.as_bytes()).map_err(|error| error.to_string())?;
    let raster = code.render::<Luma<u8>>().module_dimensions(6, 6).build();
    let size = raster.width() as f32;
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(raster)
        .write_to(&mut png, RasterImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok((
        Arc::new(Image::from_bytes(ImageFormat::Png, png.into_inner())),
        size,
    ))
}

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
    http_server: Result<HttpServer, SharedString>,
    lan_transfer_activity: TransferActivity,
    lan_transfer_active: bool,
    lan_idle_sleep_guard: Option<ActivityGuard>,
    lan_idle_sleep_request_pending: bool,
    _discovery: Option<DiscoveryService>,
    /// 当前这次连接实际使用的配置。密码框可能为空（收藏会从加密配置补密码），
    /// 成功后必须保存这里的认证信息，不能再从空白表单反推。
    pending_config: Option<SessionConfig>,
    version: String,
    available_update: Option<AvailableUpdate>,
    _subscriptions: Vec<Subscription>,
    _discovery_pump: Task<()>,
    _lan_transfer_pump: Task<()>,
}

struct SettingsDraft {
    active_tab: SettingsTab,
    profiles: Vec<SavedProfile>,
    selected_profile: Option<SavedProfile>,
    creating_profile: bool,
    profile_use_key: bool,
    profile_password_visible: bool,
    profile_key_passphrase_visible: bool,
    profile_error: Option<SharedString>,
    feedback: Option<SettingsFeedback>,
    remote_open_behavior: RemoteOpenBehavior,
    editor_path: Option<PathBuf>,
    lan_sftp_root: Option<PathBuf>,
    lan_sftp_password_mode: LanSftpPasswordMode,
    password_visible: bool,
    saving: bool,
    error: Option<SharedString>,
}

#[derive(Clone)]
enum SettingsFeedback {
    Success(SharedString),
    Error(SharedString),
}

struct MasterPasswordDraft {
    password_visible: bool,
    saving: bool,
    error: Option<SharedString>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettingsTab {
    #[default]
    Favorites,
    General,
}

impl SettingsTab {
    fn index(self) -> usize {
        match self {
            Self::Favorites => 0,
            Self::General => 1,
        }
    }
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
            name: None,
            host: config.host.clone(),
            port: config.port,
            username: config.username.clone(),
            auth,
            pinned: false,
            last_connected: Some(store::unix_timestamp()),
        },
        password,
    }
}

fn profile_password_config(profile: &SavedProfile) -> SessionConfig {
    SessionConfig {
        host: profile.host.clone(),
        port: profile.port,
        username: profile.username.clone(),
        auth: AuthMethod::Password(String::new()),
    }
}

fn favorite_password_update(
    original_auth: &SavedAuth,
    use_key: bool,
    typed: &str,
) -> Result<Option<String>, SharedString> {
    if use_key {
        return Ok(None);
    }
    if typed.is_empty() && matches!(original_auth, SavedAuth::Key { .. }) {
        return Err("切换为密码认证时，请输入新密码".into());
    }
    Ok((!typed.is_empty()).then(|| typed.to_string()))
}

fn new_favorite_profile(
    name: &str,
    host: &str,
    port: &str,
    username: &str,
    use_key: bool,
    key_path: &str,
) -> Result<SavedProfile, SharedString> {
    let host = host.trim();
    if host.is_empty() {
        return Err("请填写主机地址".into());
    }
    let port =
        parse_port(port).ok_or_else(|| SharedString::from("端口号必须是 1–65535 之间的整数"))?;
    let username = username.trim();
    if username.is_empty() {
        return Err("请填写用户名".into());
    }
    let name = name.trim();
    let auth = if use_key {
        SavedAuth::Key {
            path: expand_tilde(key_path.trim()).to_string_lossy().to_string(),
        }
    } else {
        SavedAuth::Password
    };
    Ok(SavedProfile {
        name: (!name.is_empty()).then(|| name.to_string()),
        host: host.to_string(),
        port,
        username: username.to_string(),
        auth,
        pinned: false,
        last_connected: None,
    })
}

fn parse_port(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok().filter(|port| *port > 0)
}

fn configured_lan_password(settings: &AppSettings) -> Result<Option<String>, String> {
    match settings.lan_sftp_password_mode {
        LanSftpPasswordMode::Default => Ok(None),
        LanSftpPasswordMode::Manual => store::load_lan_sftp_password().and_then(|password| {
            password
                .map(Some)
                .ok_or_else(|| "手动密码尚未设置，请在设置中填写".to_string())
        }),
    }
}

fn favorite_tooltip(profile: &SavedProfile) -> String {
    let name = profile
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("未命名");
    let auth = match &profile.auth {
        SavedAuth::Password => "认证：密码".to_string(),
        SavedAuth::Key { path } if path.trim().is_empty() => {
            "认证：私钥\n私钥：自动查找".to_string()
        }
        SavedAuth::Key { path } => format!("认证：私钥\n私钥：{path}"),
    };
    format!(
        "名称：{name}\n登录名：{}\n主机/IP：{}\n端口：{}\n{auth}\n\n双击连接",
        profile.username, profile.host, profile.port
    )
}

impl ScpApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (auto_unlocked, auto_unlock_failed) = match store::try_auto_unlock_config() {
            Ok(unlocked) => (unlocked, false),
            Err(message) => {
                log::warn!("自动解锁配置失败，将改为手动输入主密码：{message}");
                (false, true)
            }
        };
        let profiles = if auto_unlocked {
            store::load_profiles()
        } else {
            Vec::new()
        };
        let settings = if auto_unlocked {
            store::load_settings()
        } else {
            AppSettings::default()
        };
        let (lan_transfer_activity, lan_transfer_events) =
            crate::sftp::lan_server::transfer_activity();
        let server_password = auto_unlocked.then(|| configured_lan_password(&settings));
        let lan_server: Result<ServerInfo, SharedString> =
            if let Some(server_password) = &server_password {
                server_password
                    .clone()
                    .and_then(|password| {
                        crate::sftp::lan_server::start(
                            settings.lan_sftp_root.clone(),
                            &settings.lan_sftp_username,
                            password,
                            lan_transfer_activity.clone(),
                        )
                    })
                    .map_err(|message| {
                        log::error!("内置局域网 SFTP 服务启动失败：{message}");
                        SharedString::from(message)
                    })
            } else {
                Err(SharedString::from("配置尚未解锁"))
            };
        let http_server: Result<HttpServer, SharedString> =
            if let Some(server_password) = server_password {
                server_password
                    .and_then(|password| {
                        crate::http_share::start(
                            settings.lan_sftp_root.clone(),
                            &settings.lan_sftp_username,
                            password,
                            settings.lan_http_port,
                            lan_transfer_activity.clone(),
                        )
                    })
                    .map_err(|message| {
                        log::error!("浏览器文件共享服务启动失败：{message}");
                        SharedString::from(message)
                    })
            } else {
                Err(SharedString::from("配置尚未解锁"))
            };
        let toolbar = cx.new(|cx| Toolbar::new(profiles.clone(), window, cx));
        let discovery = match crate::discovery::start(
            lan_server.as_ref().ok().map(|info| info.port),
            settings.discovery_username.as_deref(),
            settings.discovery_port,
        ) {
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

                        // 连接成功才落盘凭证，避免把错误密码写进加密配置。
                        if state == SessionState::Connected && !was_connected {
                            app.persist_connection(window, cx);
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
            http_server,
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

        app.sync_window_title(window, cx);
        app.check_for_updates(cx);
        if auto_unlocked {
            app.prefill_last_profile(window, cx);
        } else {
            let this = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                Self::open_master_password_dialog(this, auto_unlock_failed, window, cx);
            });
        }
        app
    }

    fn open_master_password_dialog(
        app: WeakEntity<Self>,
        auto_unlock_failed: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let creating = !store::is_config_initialized();
        let dialog_window_handle = window.window_handle();
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if creating {
                    "设置主密码"
                } else {
                    "输入主密码"
                })
        });
        let confirmation = creating.then(|| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("再次输入主密码")
            })
        });
        let draft = cx.new(|_| MasterPasswordDraft {
            password_visible: false,
            saving: false,
            error: None,
        });
        let focus_on_mount = Rc::new(Cell::new(true));

        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_on_mount.replace(false) {
                let password = password.clone();
                window.defer(cx, move |window, cx| {
                    password.focus_handle(cx).focus(window, cx);
                });
            }

            let snapshot = draft.read(cx);
            let password_visible = snapshot.password_visible;
            let saving = snapshot.saving;
            let error = snapshot.error.clone();
            let visibility_draft = draft.clone();
            let visibility_password = password.clone();
            let visibility_confirmation = confirmation.clone();
            let submit_draft = draft.clone();
            let submit_password = password.clone();
            let submit_confirmation = confirmation.clone();
            let submit_app = app.clone();
            let skip_unlock = app.clone();

            dialog
                .title(if creating {
                    "设置 Blip 主密码"
                } else {
                    "解锁 Blip 配置"
                })
                .w(px(480.))
                .close_button(false)
                .overlay_closable(false)
                .keyboard(false)
                .child(
                    v_flex()
                        .w_full()
                        .gap_3()
                        .child(div().text_sm().child(if creating {
                            "主密码用于加密收藏、应用设置和保存的连接凭据。"
                        } else {
                            "输入这一次主密码后，本机以后将自动解锁配置。"
                        }))
                        .when(auto_unlock_failed, |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().warning)
                                    .child(
                                        "本机自动解锁信息已失效。输入主密码可重新绑定本机；如果仍失败，配置正文可能已损坏。",
                                    ),
                            )
                        })
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .child(Input::new(&password).disabled(saving).w_full()),
                                )
                                .child(
                                    Button::new("master-password-visibility")
                                        .ghost()
                                        .icon(if password_visible {
                                            IconName::EyeOff
                                        } else {
                                            IconName::Eye
                                        })
                                        .accessibility_label(if password_visible {
                                            "隐藏主密码"
                                        } else {
                                            "显示主密码"
                                        })
                                        .disabled(saving)
                                        .on_click(move |_, window, cx| {
                                            let visible =
                                                !visibility_draft.read(cx).password_visible;
                                            visibility_password.update(cx, |input, cx| {
                                                input.set_masked(!visible, window, cx)
                                            });
                                            if let Some(confirmation) = &visibility_confirmation {
                                                confirmation.update(cx, |input, cx| {
                                                    input.set_masked(!visible, window, cx)
                                                });
                                            }
                                            visibility_draft.update(cx, |draft, cx| {
                                                draft.password_visible = visible;
                                                cx.notify();
                                            });
                                        }),
                                ),
                        )
                        .when_some(confirmation.clone(), |this, confirmation| {
                            this.child(Input::new(&confirmation).disabled(saving).w_full())
                        })
                        .when_some(error, |this, error| {
                            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                        })
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if creating {
                                    "Blip 会把解锁密钥绑定到本机和当前用户；复制配置到其他电脑仍需主密码。"
                                } else {
                                    "解锁信息保存在 ~/.ssh/.blip-unlock-key，不使用系统密码存储。"
                                }),
                        ),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .when(!creating, |this| {
                            this.child(
                                Button::new("master-password-skip")
                                    .label("暂不解锁")
                                    .disabled(saving)
                                    .on_click(move |_, window, cx| {
                                        let _ = skip_unlock.update(cx, |app, cx| {
                                            app.toolbar.update(cx, |toolbar, cx| {
                                                toolbar.set_error(
                                                    Some(
                                                        "配置未解锁：收藏、设置和保存的凭据暂不可用"
                                                            .into(),
                                                    ),
                                                    cx,
                                                )
                                            });
                                        });
                                        window.close_dialog(cx);
                                        window.push_notification(
                                            Notification::warning(
                                                "已进入未解锁模式；仍可手动填写连接信息",
                                            ),
                                            cx,
                                        );
                                    }),
                            )
                        })
                        .child(
                            Button::new("master-password-submit")
                                .primary()
                                .label(if creating {
                                    "创建并解锁"
                                } else if auto_unlock_failed {
                                    "解锁并修复"
                                } else {
                                    "解锁"
                                })
                                .loading(saving)
                                .disabled(saving)
                                .on_click(move |_, _window, cx| {
                                let password = submit_password.read(cx).value().to_string();
                                if password.is_empty() {
                                    submit_draft.update(cx, |draft, cx| {
                                        draft.error = Some("主密码不能为空".into());
                                        cx.notify();
                                    });
                                    return;
                                }
                                if let Some(confirmation) = &submit_confirmation
                                    && confirmation.read(cx).value() != password.as_str()
                                {
                                    submit_draft.update(cx, |draft, cx| {
                                        draft.error = Some("两次输入的主密码不一致".into());
                                        cx.notify();
                                    });
                                    return;
                                }

                                submit_draft.update(cx, |draft, cx| {
                                    draft.saving = true;
                                    draft.error = None;
                                    cx.notify();
                                });
                                let submit_draft = submit_draft.clone();
                                let submit_app = submit_app.clone();
                                cx.spawn(async move |cx| {
                                    let result = cx
                                        .background_executor()
                                        .spawn(async move {
                                            if creating {
                                                store::initialize_config(&password)
                                            } else {
                                                store::unlock_config(&password)
                                            }
                                        })
                                        .await;
                                    match result {
                                        Ok(()) => {
                                            let _ = cx.update_window(
                                                dialog_window_handle,
                                                |_, window, cx| {
                                                    let _ = submit_app.update(cx, |app, cx| {
                                                        app.apply_unlocked_config(window, cx)
                                                    });
                                                    window.close_dialog(cx);
                                                },
                                            );
                                        }
                                        Err(message) => {
                                            submit_draft.update(cx, |draft, cx| {
                                                draft.saving = false;
                                                draft.error = Some(message.into());
                                                cx.notify();
                                            });
                                        }
                                    }
                                })
                                .detach();
                                }),
                        ),
                )
        });
    }

    fn apply_unlocked_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profiles = store::load_profiles();
        let settings = store::load_settings();
        let server_password = configured_lan_password(&settings);
        self.lan_server = server_password
            .clone()
            .and_then(|password| {
                crate::sftp::lan_server::start(
                    settings.lan_sftp_root.clone(),
                    &settings.lan_sftp_username,
                    password,
                    self.lan_transfer_activity.clone(),
                )
            })
            .map_err(|message| {
                log::error!("内置局域网 SFTP 服务启动失败：{message}");
                SharedString::from(message)
            });
        self.http_server = server_password
            .and_then(|password| {
                crate::http_share::start(
                    settings.lan_sftp_root.clone(),
                    &settings.lan_sftp_username,
                    password,
                    settings.lan_http_port,
                    self.lan_transfer_activity.clone(),
                )
            })
            .map_err(|message| {
                log::error!("浏览器文件共享服务启动失败：{message}");
                SharedString::from(message)
            });
        if let Ok(info) = &self.lan_server {
            log::info!(
                "局域网 SFTP 已启动：{}@{}，共享目录 {}",
                info.username(),
                info.endpoint(),
                info.root().display()
            );
        }
        if let Some(discovery) = &self._discovery {
            if let Err(message) = discovery.set_discovery_port(settings.discovery_port) {
                log::warn!("更新 UDP 设备发现端口失败：{message}");
                window.push_notification(
                    Notification::error(format!("配置已解锁，但发现端口应用失败：{message}")),
                    cx,
                );
            }
            if let Err(message) = discovery.set_username(settings.discovery_username.as_deref()) {
                log::warn!("更新设备发现用户名失败：{message}");
            }
            let port = self.lan_server.as_ref().ok().map(|server| server.port);
            if let Err(message) = discovery.set_port(port) {
                log::warn!("更新设备发现服务端口失败：{message}");
            }
        }

        self.profiles = profiles.clone();
        self.settings = settings.clone();
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.set_profiles(profiles, cx));
        self.workspace
            .update(cx, |workspace, cx| workspace.set_settings(settings, cx));
        self.prefill_last_profile(window, cx);
        if let Err(message) = &self.lan_server {
            window.push_notification(
                Notification::error(format!("配置已解锁，但局域网 SFTP 启动失败：{message}")),
                cx,
            );
        }
        if let Err(message) = &self.http_server {
            window.push_notification(
                Notification::error(format!("配置已解锁，但浏览器文件共享启动失败：{message}")),
                cx,
            );
        }
    }

    fn apply_http_settings(
        &mut self,
        settings: &AppSettings,
        manual_password: Option<String>,
        effective_root: PathBuf,
    ) -> Result<(), String> {
        if let Ok(server) = &self.http_server
            && server.port() == settings.lan_http_port
        {
            server.set_username(&settings.lan_sftp_username)?;
            server.set_root(effective_root)?;
            match settings.lan_sftp_password_mode {
                LanSftpPasswordMode::Manual => server.set_password(manual_password)?,
                LanSftpPasswordMode::Default
                    if self.settings.lan_sftp_password_mode == LanSftpPasswordMode::Manual =>
                {
                    server.set_password(None)?
                }
                LanSftpPasswordMode::Default => {}
            }
            return Ok(());
        }

        match crate::http_share::start(
            settings.lan_sftp_root.clone(),
            &settings.lan_sftp_username,
            manual_password,
            settings.lan_http_port,
            self.lan_transfer_activity.clone(),
        ) {
            Ok(server) => {
                self.http_server = Ok(server);
                Ok(())
            }
            Err(message) => {
                if self.http_server.is_err() {
                    self.http_server = Err(message.clone().into());
                }
                Err(message)
            }
        }
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
        let request = cx.prevent_idle_sleep("Blip 局域网正在传输文件");
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

    fn open_http_server_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let server = self
            .http_server
            .as_ref()
            .map(|server| {
                (
                    server.url(),
                    server.username(),
                    server.password(),
                    server.root(),
                )
            })
            .map_err(Clone::clone);
        window.open_dialog(cx, move |dialog, _window, cx| match server.clone() {
            Ok((url, username, password, root)) => {
                let theme = cx.theme().clone();
                let qr_code = qr_code_image(&url);
                let copy_url = url.clone();
                let open_url = url.clone();
                let copy_password = password.clone();
                dialog
                    .title("浏览器文件共享")
                    .w(px(560.))
                    .overlay_closable(false)
                    .child(
                        v_flex()
                            .w_full()
                            .gap_3()
                            .child(
                                div()
                                    .text_sm()
                                    .child("同一局域网内的设备可在浏览器中打开以下地址。"),
                            )
                            .child(match qr_code {
                                Ok((image, size)) => v_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(img(image).size(px(size)))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("手机扫码打开"),
                                    )
                                    .into_any_element(),
                                Err(error) => {
                                    log::warn!("生成 HTTP 共享二维码失败：{error}");
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("二维码生成失败，可复制访问地址")
                                        .into_any_element()
                                }
                            })
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
                                                    .child("访问地址"),
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
                                                    .child(SharedString::from(url)),
                                            )
                                            .child(
                                                Button::new("copy-http-url")
                                                    .label("复制")
                                                    .on_click(move |_, window, cx| {
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(
                                                                copy_url.clone(),
                                                            ),
                                                        );
                                                        window.push_notification(
                                                            Notification::success("访问地址已复制"),
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
                                                    .child("用户名"),
                                            )
                                            .child(
                                                div().text_sm().child(SharedString::from(username)),
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
                                                Button::new("copy-http-password")
                                                    .label("复制")
                                                    .on_click(move |_, window, cx| {
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(
                                                                copy_password.clone(),
                                                            ),
                                                        );
                                                        window.push_notification(
                                                            Notification::success("登录密码已复制"),
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
                                                    .child(SharedString::from(display_local_path(
                                                        &root,
                                                    ))),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("支持文件与文件夹上传；请只在可信局域网中使用。"),
                            ),
                    )
                    .footer(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("close-http-share")
                                    .label("关闭")
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(
                                Button::new("open-http-share")
                                    .primary()
                                    .label("打开")
                                    .on_click(move |_, _, cx| cx.open_url(&open_url)),
                            ),
                    )
            }
            Err(message) => dialog
                .title("浏览器文件共享启动失败")
                .w(px(480.))
                .overlay_closable(false)
                .child(
                    v_flex().gap_2().child(message).child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("请在设置中检查 HTTP 端口和共享目录，保存后会重新启动服务。"),
                    ),
                )
                .footer(
                    h_flex().justify_end().child(
                        Button::new("close-http-share-error")
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

    /// 连接成功后自动加入收藏，并按“记住密码”偏好保存本次实际使用的密码。
    fn persist_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.pending_config.clone() else {
            log::warn!("连接成功但缺少实际连接配置，跳过连接持久化");
            return;
        };
        let remember = self.toolbar.read(cx).snapshot(cx).remember;
        let prepared = prepare_profile_persistence(&config);
        let existing = self
            .profiles
            .iter()
            .find(|profile| {
                profile.host == config.host
                    && profile.port == config.port
                    && profile.username == config.username
            })
            .cloned();
        let mut updated = prepared.profile.clone();
        if let Some(existing) = &existing {
            updated.name = existing.name.clone();
            updated.pinned = existing.pinned;
        }
        let mut profiles = self.profiles.clone();
        let result = store::upsert_profile(&mut profiles, updated);
        let profile_retained = profiles.iter().any(|profile| {
            profile.host == config.host
                && profile.port == config.port
                && profile.username == config.username
        });
        let previous = profile_retained.then_some(result.previous).flatten();
        let evicted = result.evicted;
        let has_secret = match &config.auth {
            AuthMethod::Password(_) => prepared.password.is_some(),
            AuthMethod::PrivateKey { passphrase, .. } => passphrase.is_some(),
        };
        let remember = remember && profile_retained && has_secret;

        // age 的主密码派生是 CPU 密集型工作。收藏和凭据合并成一次写入，并在
        // 后台完成；连接成功后 UI 可以立即继续响应。
        cx.spawn_in(window, async move |this, cx| {
            let persisted_profiles = profiles.clone();
            let persisted_evicted = evicted.clone();
            let task = cx.background_executor().spawn(async move {
                store::save_connection(
                    &persisted_profiles,
                    &config,
                    remember,
                    previous.as_ref(),
                    persisted_evicted.as_ref(),
                )?;
                Ok::<_, String>((persisted_profiles, persisted_evicted))
            });

            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| match result {
                Ok((profiles, evicted)) => {
                    if let Some(evicted) = evicted {
                        window.push_notification(
                            Notification::info(format!(
                                "收藏已达 50 条，已移除最久未使用的“{}”",
                                evicted.title()
                            )),
                            cx,
                        );
                    }
                    app.profiles = profiles.clone();
                    app.toolbar
                        .update(cx, |toolbar, cx| toolbar.set_profiles(profiles, cx));
                }
                Err(err) => {
                    log::warn!("自动保存收藏失败：{err}");
                    app.toolbar.update(cx, |toolbar, cx| {
                        toolbar
                            .set_error(Some(format!("连接成功，但保存收藏失败：{err}").into()), cx)
                    });
                }
            });
        })
        .detach();
    }

    fn persist_installed_key(&mut self, private_key: &std::path::Path, cx: &mut Context<Self>) {
        let Some(config) = self.pending_config.as_mut() else {
            self.toolbar.update(cx, |toolbar, cx| {
                toolbar.set_error(Some("公钥已部署，但无法更新本地连接配置".into()), cx)
            });
            return;
        };
        config.auth = AuthMethod::PrivateKey {
            path: private_key.to_path_buf(),
            passphrase: None,
        };
        let Some(existing) = self
            .profiles
            .iter()
            .find(|profile| {
                profile.host == config.host
                    && profile.port == config.port
                    && profile.username == config.username
            })
            .cloned()
        else {
            return;
        };

        let mut profiles = self.profiles.clone();
        store::upsert_profile(
            &mut profiles,
            SavedProfile {
                name: existing.name.clone(),
                host: config.host.clone(),
                port: config.port,
                username: config.username.clone(),
                auth: SavedAuth::Key {
                    path: private_key.to_string_lossy().to_string(),
                },
                pinned: existing.pinned,
                last_connected: Some(store::unix_timestamp()),
            },
        );
        if let Err(err) = store::save_profile_removal(&profiles, &existing) {
            log::warn!("保存密钥连接配置失败：{err}");
            self.toolbar.update(cx, |toolbar, cx| {
                toolbar.set_error(
                    Some(SharedString::from(format!(
                        "公钥已部署，但保存连接配置失败：{err}"
                    ))),
                    cx,
                )
            });
            return;
        }
        self.profiles = profiles.clone();
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.set_profiles(profiles, cx));
    }

    /// 启动时把最近一次成功的连接填回表单。
    fn prefill_last_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self
            .profiles
            .iter()
            .filter(|profile| profile.last_connected.is_some())
            .max_by_key(|profile| profile.last_connected)
            .or_else(|| self.profiles.first())
            .cloned()
        else {
            return;
        };
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.prefill(&profile, window, cx));
    }

    fn window_title(&self, cx: &App) -> String {
        let toolbar = self.toolbar.read(cx);
        match toolbar.state() {
            SessionState::Connected => {
                let snapshot = toolbar.snapshot(cx);
                format!("Blip — {}@{}", snapshot.username, snapshot.host)
            }
            SessionState::Connecting => "Blip — 正在连接…".to_string(),
            SessionState::Disconnected => "Blip".to_string(),
        }
    }

    /// 把连接状态写进窗口标题栏：`Blip — root@192.168.146.7`。
    ///
    /// 这串 `user@host` 原来贴在表单行的最右边，和输入框挤在一起，
    /// 长得就像一个输入框；挪到窗口顶部后，表单那一行也宽松了。
    /// 断开时退回纯 `Blip`。
    fn sync_window_title(&self, window: &mut Window, cx: &App) {
        window.set_window_title(&self.window_title(cx));
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password_mode = self.settings.lan_sftp_password_mode;
        let profiles = self.toolbar.read(cx).profiles_for_display();
        let (manual_password, password_error) = match password_mode {
            LanSftpPasswordMode::Default => (String::new(), None),
            LanSftpPasswordMode::Manual => match store::load_lan_sftp_password() {
                Ok(password) => (password.unwrap_or_default(), None),
                Err(message) => (String::new(), Some(SharedString::from(message))),
            },
        };
        let manual_password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(manual_password)
                .masked(true)
                .placeholder("输入手动密码")
        });
        let discovery_username_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(self.settings.discovery_username.clone().unwrap_or_default())
                .placeholder("留空则显示计算机名")
        });
        let lan_sftp_username_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(self.settings.lan_sftp_username.clone())
                .placeholder("SFTP 登录用户名")
        });
        let discovery_port_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(self.settings.discovery_port.to_string())
                .placeholder("1–65535")
        });
        let lan_http_port_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(self.settings.lan_http_port.to_string())
                .placeholder("1–65535")
        });
        let profile_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("名称（可选）"));
        let profile_host_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("主机名或 IP"));
        let profile_port_input = cx.new(|cx| InputState::new(window, cx).placeholder("1–65535"));
        let profile_username_input = cx.new(|cx| InputState::new(window, cx).placeholder("用户名"));
        let profile_key_path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("私钥文件路径；留空则自动查找"));
        let profile_password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("留空则保留原密码")
        });
        let profile_key_passphrase_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("留空则保留原密钥口令")
        });
        let draft = cx.new(|_| SettingsDraft {
            active_tab: SettingsTab::Favorites,
            profiles,
            selected_profile: None,
            creating_profile: false,
            profile_use_key: false,
            profile_password_visible: false,
            profile_key_passphrase_visible: false,
            profile_error: None,
            feedback: None,
            remote_open_behavior: self.settings.remote_open_behavior,
            editor_path: self.settings.editor_path.clone(),
            lan_sftp_root: self.settings.lan_sftp_root.clone(),
            lan_sftp_password_mode: password_mode,
            password_visible: false,
            saving: false,
            error: if password_mode == LanSftpPasswordMode::Manual {
                password_error
            } else {
                None
            },
        });
        let app = cx.entity().downgrade();
        let toolbar = self.toolbar.clone();

        window.open_dialog(cx, move |dialog, dialog_window, cx| {
            let theme = cx.theme().clone();
            let dialog_window_handle = dialog_window.window_handle();
            let (
                active_tab,
                profiles,
                selected_profile,
                creating_profile,
                profile_use_key,
                profile_password_visible,
                profile_key_passphrase_visible,
                profile_error,
                feedback,
                selected,
                editor_path,
                lan_sftp_root,
                lan_sftp_password_mode,
                password_visible,
                saving,
                error,
            ) = {
                let snapshot = draft.read(cx);
                (
                    snapshot.active_tab,
                    snapshot.profiles.clone(),
                    snapshot.selected_profile.clone(),
                    snapshot.creating_profile,
                    snapshot.profile_use_key,
                    snapshot.profile_password_visible,
                    snapshot.profile_key_passphrase_visible,
                    snapshot.profile_error.clone(),
                    snapshot.feedback.clone(),
                    match snapshot.remote_open_behavior {
                        RemoteOpenBehavior::SystemDefault => 0,
                        RemoteOpenBehavior::CustomEditor => 1,
                        RemoteOpenBehavior::BuiltInTextEditor => 2,
                    },
                    snapshot.editor_path.clone(),
                    snapshot.lan_sftp_root.clone(),
                    snapshot.lan_sftp_password_mode,
                    snapshot.password_visible,
                    snapshot.saving,
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
            let save_discovery_username_input = discovery_username_input.clone();
            let save_discovery_port_input = discovery_port_input.clone();
            let save_lan_http_port_input = lan_http_port_input.clone();
            let save_lan_sftp_username_input = lan_sftp_username_input.clone();
            let save_app = app.clone();
            let tabs_draft = draft.clone();
            let feedback_draft = draft.clone();

            let add_profile_draft = draft.clone();
            let add_profile_name_input = profile_name_input.clone();
            let add_profile_host_input = profile_host_input.clone();
            let add_profile_port_input = profile_port_input.clone();
            let add_profile_username_input = profile_username_input.clone();
            let add_profile_key_path_input = profile_key_path_input.clone();
            let add_profile_password_input = profile_password_input.clone();
            let add_profile_key_passphrase_input = profile_key_passphrase_input.clone();
            let favorite_auth_draft = draft.clone();
            let favorite_password_visibility_draft = draft.clone();
            let favorite_password_visibility_input = profile_password_input.clone();
            let favorite_key_passphrase_visibility_draft = draft.clone();
            let favorite_key_passphrase_visibility_input = profile_key_passphrase_input.clone();
            let choose_profile_key_input = profile_key_path_input.clone();
            let choose_profile_key_draft = draft.clone();
            let favorites = h_flex()
                .items_stretch()
                .w_full()
                .flex_1()
                .min_h_0()
                .child(
                    v_flex()
                        .h_full()
                        .w(px(260.))
                        .flex_shrink_0()
                        .gap_3()
                        .pr_4()
                        .border_r_1()
                        .border_color(theme.border)
                        .child(
                            h_flex()
                                .justify_between()
                                .gap_2()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(Icon::new(IconName::Bookmark).xsmall())
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child("收藏"),
                                        ),
                                )
                                .child(
                                    Button::new("settings-add-favorite")
                                        .small()
                                        .icon(IconName::BookmarkPlus)
                                        .label("添加…")
                                        .on_click(move |_, window, cx| {
                                            add_profile_name_input.update(cx, |input, cx| {
                                                input.set_value(String::new(), window, cx)
                                            });
                                            add_profile_host_input.update(cx, |input, cx| {
                                                input.set_value(String::new(), window, cx)
                                            });
                                            add_profile_port_input.update(cx, |input, cx| {
                                                input.set_value("22", window, cx)
                                            });
                                            add_profile_username_input.update(cx, |input, cx| {
                                                input.set_value(String::new(), window, cx)
                                            });
                                            add_profile_key_path_input.update(cx, |input, cx| {
                                                input.set_value(String::new(), window, cx)
                                            });
                                            add_profile_password_input.update(cx, |input, cx| {
                                                input.set_value(String::new(), window, cx);
                                                input.set_masked(true, window, cx);
                                                input.set_placeholder("输入密码", window, cx);
                                            });
                                            add_profile_key_passphrase_input.update(
                                                cx,
                                                |input, cx| {
                                                    input.set_value(
                                                        String::new(),
                                                        window,
                                                        cx,
                                                    );
                                                    input.set_masked(true, window, cx);
                                                    input.set_placeholder(
                                                        "口令（可选）",
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            );
                                            add_profile_draft.update(cx, |draft, cx| {
                                                draft.selected_profile = Some(SavedProfile {
                                                    name: None,
                                                    host: String::new(),
                                                    port: 22,
                                                    username: String::new(),
                                                    auth: SavedAuth::Password,
                                                    pinned: false,
                                                    last_connected: None,
                                                });
                                                draft.creating_profile = true;
                                                draft.profile_use_key = false;
                                                draft.profile_password_visible = false;
                                                draft.profile_key_passphrase_visible = false;
                                                draft.profile_error = None;
                                            draft.feedback = None;
                                                cx.notify();
                                            });
                                            add_profile_host_input
                                                .update(cx, |input, cx| input.focus(window, cx));
                                        }),
                                ),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_h_0()
                                .gap_1()
                                .overflow_y_scrollbar()
                                .when(profiles.is_empty(), |this| {
                                    this.child(
                                        v_flex()
                                            .flex_1()
                                            .items_center()
                                            .justify_center()
                                            .gap_1()
                                            .text_center()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child("暂无收藏"),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child("成功连接后会自动收藏，也可手动添加"),
                                            ),
                                    )
                                })
                                .children(profiles.clone().into_iter().map(|profile| {
                                    let key = profile.endpoint_label();
                                    let title = profile.title();
                                    let endpoint = profile.endpoint_label();
                                    let tooltip = favorite_tooltip(&profile);
                                    let auth_icon = match profile.auth {
                                        SavedAuth::Password => IconName::Lock,
                                        SavedAuth::Key { .. } => IconName::KeyRound,
                                    };
                                    let is_selected = selected_profile.as_ref() == Some(&profile);
                                    let select_draft = draft.clone();
                                    let select_name_input = profile_name_input.clone();
                                    let select_host_input = profile_host_input.clone();
                                    let select_port_input = profile_port_input.clone();
                                    let select_username_input = profile_username_input.clone();
                                    let select_key_path_input = profile_key_path_input.clone();
                                    let select_password_input = profile_password_input.clone();
                                    let select_key_passphrase_input =
                                        profile_key_passphrase_input.clone();
                                    let select_profile = profile.clone();
                                    let list_connect_toolbar = toolbar.clone();
                                    let list_connect_profile = profile.clone();

                                    Button::new(format!("settings-favorite-{key}"))
                                        .ghost()
                                        .small()
                                        .w_full()
                                        .min_w_0()
                                        .justify_start()
                                        .selected(is_selected)
                                        .child(
                                            h_flex()
                                                .w_full()
                                                .min_w_0()
                                                .gap_2()
                                                .child(Icon::new(auth_icon).xsmall())
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .text_left()
                                                        .whitespace_nowrap()
                                                        .text_ellipsis()
                                                        .child(SharedString::from(title.clone())),
                                                )
                                                .when(profile.pinned, |this| {
                                                    this.child(Icon::new(IconName::Pin).xsmall())
                                                }),
                                        )
                                        .accessibility_label(SharedString::from(format!(
                                            "选择收藏 {title}，{endpoint}；双击连接"
                                        )))
                                        .tooltip(SharedString::from(tooltip))
                                        .on_click(move |event, window, cx| {
                                            if event.click_count() == 2 {
                                                list_connect_toolbar.update(
                                                    cx,
                                                    |toolbar, cx| {
                                                        toolbar.connect_profile(
                                                            &list_connect_profile,
                                                            window,
                                                            cx,
                                                        )
                                                    },
                                                );
                                                window.close_dialog(cx);
                                                return;
                                            }
                                            select_name_input.update(cx, |input, cx| {
                                                input.set_value(
                                                    select_profile
                                                        .name
                                                        .clone()
                                                        .unwrap_or_default(),
                                                    window,
                                                    cx,
                                                )
                                            });
                                            select_host_input.update(cx, |input, cx| {
                                                input.set_value(
                                                    select_profile.host.clone(),
                                                    window,
                                                    cx,
                                                )
                                            });
                                            select_port_input.update(cx, |input, cx| {
                                                input.set_value(
                                                    select_profile.port.to_string(),
                                                    window,
                                                    cx,
                                                )
                                            });
                                            select_username_input.update(cx, |input, cx| {
                                                input.set_value(
                                                    select_profile.username.clone(),
                                                    window,
                                                    cx,
                                                )
                                            });
                                            select_key_path_input.update(cx, |input, cx| {
                                                let path = match &select_profile.auth {
                                                    SavedAuth::Key { path } => path.clone(),
                                                    SavedAuth::Password => String::new(),
                                                };
                                                input.set_value(path, window, cx);
                                            });
                                            select_password_input.update(cx, |input, cx| {
                                                input.set_value(String::new(), window, cx);
                                                input.set_masked(true, window, cx);
                                                input.set_placeholder(
                                                    "留空则保留原密码",
                                                    window,
                                                    cx,
                                                );
                                            });
                                            select_key_passphrase_input.update(
                                                cx,
                                                |input, cx| {
                                                    input.set_value(
                                                        String::new(),
                                                        window,
                                                        cx,
                                                    );
                                                    input.set_masked(true, window, cx);
                                                    input.set_placeholder(
                                                        "留空则保留原密钥口令",
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            );
                                            select_draft.update(cx, |draft, cx| {
                                                draft.creating_profile = false;
                                                draft.profile_use_key = matches!(
                                                    select_profile.auth,
                                                    SavedAuth::Key { .. }
                                                );
                                                draft.selected_profile =
                                                    Some(select_profile.clone());
                                                draft.profile_password_visible = false;
                                                draft.profile_key_passphrase_visible = false;
                                                draft.profile_error = None;
                                                draft.feedback = None;
                                                cx.notify();
                                            });
                                        })
                                })),
                        ),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .pl_5()
                        .when(selected_profile.is_none(), |this| {
                            this.items_center().justify_center().gap_2().child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("选择左侧收藏以查看和编辑"),
                            )
                        })
                        .when_some(selected_profile.clone(), |this, profile| {
                            let password_hint = if creating_profile {
                                "密码会保存到主密码保护的配置中。"
                            } else if matches!(&profile.auth, SavedAuth::Password) {
                                "留空则保留加密配置中的原密码。"
                            } else {
                                "从私钥切换为密码认证时，请输入新密码。"
                            };
                            this.gap_4()
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(if creating_profile {
                                                    "添加收藏"
                                                } else {
                                                    "收藏详情"
                                                }),
                                        )
                                        .when(!creating_profile, |this| this.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(SharedString::from(profile.endpoint_label())),
                                        )),
                                )
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(div().text_xs().child("名称"))
                                        .child(
                                            Input::new(&profile_name_input)
                                                .id("favorite-profile-name")
                                                .w_full(),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_3()
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_2()
                                                .child(div().text_xs().child("主机名"))
                                                .child(
                                                    Input::new(&profile_host_input)
                                                        .id("favorite-profile-host")
                                                        .w_full(),
                                                ),
                                        )
                                        .child(
                                            v_flex()
                                                .w(px(150.))
                                                .gap_2()
                                                .child(div().text_xs().child("端口"))
                                                .child(
                                                    Input::new(&profile_port_input)
                                                        .id("favorite-profile-port")
                                                        .w_full(),
                                                ),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(div().text_xs().child("用户名"))
                                        .child(
                                            Input::new(&profile_username_input)
                                                .id("favorite-profile-username")
                                                .w_full(),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(div().text_xs().child("认证方式"))
                                        .child(
                                            RadioGroup::horizontal("favorite-auth-method")
                                                .selected_index(Some(if profile_use_key {
                                                    1
                                                } else {
                                                    0
                                                }))
                                                .on_click(move |index, _, cx| {
                                                    favorite_auth_draft.update(
                                                        cx,
                                                        |draft, cx| {
                                                            draft.profile_use_key = *index == 1;
                                                            draft.profile_error = None;
                                                            cx.notify();
                                                        },
                                                    );
                                                })
                                                .child(
                                                    Radio::new("favorite-auth-password")
                                                        .label("密码"),
                                                )
                                                .child(
                                                    Radio::new("favorite-auth-key")
                                                        .label("私钥"),
                                                ),
                                        ),
                                )
                                .when(!profile_use_key, |this| {
                                    this.child(
                                        v_flex()
                                            .gap_2()
                                            .child(div().text_xs().child("密码"))
                                            .child(
                                                h_flex()
                                                    .w_full()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .child(
                                                                Input::new(
                                                                    &profile_password_input,
                                                                )
                                                                .id("favorite-profile-password")
                                                                .w_full(),
                                                            ),
                                                    )
                                                    .child(
                                                        Button::new(
                                                            "favorite-password-visibility",
                                                        )
                                                        .label(if profile_password_visible {
                                                            "隐藏"
                                                        } else {
                                                            "显示"
                                                        })
                                                        .on_click(move |_, window, cx| {
                                                            let visible =
                                                                !favorite_password_visibility_draft
                                                                    .read(cx)
                                                                    .profile_password_visible;
                                                            favorite_password_visibility_input
                                                                .update(cx, |input, cx| {
                                                                    input.set_masked(
                                                                        !visible, window, cx,
                                                                    )
                                                                });
                                                            favorite_password_visibility_draft
                                                                .update(cx, |draft, cx| {
                                                                    draft.profile_password_visible =
                                                                        visible;
                                                                    cx.notify();
                                                                });
                                                        }),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(password_hint),
                                            ),
                                    )
                                })
                                .when(profile_use_key, |this| {
                                    this.child(
                                        v_flex()
                                            .gap_2()
                                            .child(div().text_xs().child("密钥位置"))
                                            .child(
                                                h_flex()
                                                    .w_full()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .child(
                                                                Input::new(
                                                                    &profile_key_path_input,
                                                                )
                                                                .id(
                                                                    "favorite-profile-key-path",
                                                                )
                                                                .w_full(),
                                                            ),
                                                    )
                                                    .child(
                                                        Button::new("choose-favorite-key-file")
                                                            .label("选择文件…")
                                                            .on_click(move |_, _, cx| {
                                                                #[cfg(target_os = "macos")]
                                                                {
                                                                    let current =
                                                                        choose_profile_key_input
                                                                            .read(cx)
                                                                            .value()
                                                                            .trim()
                                                                            .to_string();
                                                                    let expanded =
                                                                        expand_tilde(&current);
                                                                    let initial = crate::ui::toolbar::private_key_initial_directory(
                                                                        (!current.is_empty())
                                                                            .then_some(expanded.as_path()),
                                                                    );
                                                                    let receiver = crate::ui::toolbar::prompt_for_key_file(
                                                                        "选择 SSH 私钥文件",
                                                                        true,
                                                                        initial.as_deref(),
                                                                    );
                                                                    let input =
                                                                        choose_profile_key_input
                                                                            .clone();
                                                                    let draft =
                                                                        choose_profile_key_draft
                                                                            .clone();
                                                                    cx.spawn(async move |cx| {
                                                                        match receiver
                                                                            .recv_async()
                                                                            .await
                                                                        {
                                                                            Ok(Ok(selection)) => {
                                                                                if let Some(path) =
                                                                                    selection.path
                                                                                {
                                                                                    let _ = cx.update_window(
                                                                                        dialog_window_handle,
                                                                                        |_, window, cx| {
                                                                                            input.update(cx, |input, cx| {
                                                                                                input.set_value(
                                                                                                    path.to_string_lossy().to_string(),
                                                                                                    window,
                                                                                                    cx,
                                                                                                )
                                                                                            });
                                                                                        },
                                                                                    );
                                                                                    draft.update(
                                                                                        cx,
                                                                                        |draft, cx| {
                                                                                            draft.profile_error = None;
                                                                                            cx.notify();
                                                                                        },
                                                                                    );
                                                                                }
                                                                            }
                                                                            Ok(Err(message)) => {
                                                                                draft.update(
                                                                                    cx,
                                                                                    |draft, cx| {
                                                                                        draft.profile_error = Some(
                                                                                            SharedString::from(message),
                                                                                        );
                                                                                        cx.notify();
                                                                                    },
                                                                                );
                                                                            }
                                                                            Err(_) => {
                                                                                draft.update(
                                                                                    cx,
                                                                                    |draft, cx| {
                                                                                        draft.profile_error = Some(
                                                                                            "私钥选择器意外关闭，请重试"
                                                                                                .into(),
                                                                                        );
                                                                                        cx.notify();
                                                                                    },
                                                                                );
                                                                            }
                                                                        }
                                                                    })
                                                                    .detach();
                                                                }
                                                                #[cfg(not(target_os = "macos"))]
                                                                {
                                                                    let receiver = cx.prompt_for_paths(
                                                                        PathPromptOptions {
                                                                            files: true,
                                                                            directories: false,
                                                                            multiple: false,
                                                                            prompt: Some(
                                                                                "选择 SSH 私钥文件"
                                                                                    .into(),
                                                                            ),
                                                                        },
                                                                    );
                                                                    let input =
                                                                        choose_profile_key_input
                                                                            .clone();
                                                                    let draft =
                                                                        choose_profile_key_draft
                                                                            .clone();
                                                                    cx.spawn(async move |cx| {
                                                                        match receiver.await {
                                                                            Ok(Ok(Some(paths))) => {
                                                                                if let Some(path) =
                                                                                    paths.into_iter().next()
                                                                                {
                                                                                    let _ = cx.update_window(
                                                                                        dialog_window_handle,
                                                                                        |_, window, cx| {
                                                                                            input.update(cx, |input, cx| {
                                                                                                input.set_value(
                                                                                                    path.to_string_lossy().to_string(),
                                                                                                    window,
                                                                                                    cx,
                                                                                                )
                                                                                            });
                                                                                        },
                                                                                    );
                                                                                    draft.update(
                                                                                        cx,
                                                                                        |draft, cx| {
                                                                                            draft.profile_error = None;
                                                                                            cx.notify();
                                                                                        },
                                                                                    );
                                                                                }
                                                                            }
                                                                            Ok(Err(error)) => {
                                                                                draft.update(
                                                                                    cx,
                                                                                    |draft, cx| {
                                                                                        draft.profile_error = Some(
                                                                                            format!(
                                                                                                "无法选择私钥：{error}"
                                                                                            )
                                                                                            .into(),
                                                                                        );
                                                                                        cx.notify();
                                                                                    },
                                                                                );
                                                                            }
                                                                            Ok(Ok(None)) => {}
                                                                            Err(_) => {
                                                                                draft.update(
                                                                                    cx,
                                                                                    |draft, cx| {
                                                                                        draft.profile_error = Some(
                                                                                            "私钥选择器意外关闭，请重试"
                                                                                                .into(),
                                                                                        );
                                                                                        cx.notify();
                                                                                    },
                                                                                );
                                                                            }
                                                                        }
                                                                    })
                                                                    .detach();
                                                                }
                                                            }),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child("支持 ~/ 开头的路径；留空时自动查找 ~/.ssh 中的私钥。"),
                                            )
                                            .child(
                                                v_flex()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .child("密钥口令（可选）"),
                                                    )
                                                    .child(
                                                        h_flex()
                                                            .w_full()
                                                            .gap_2()
                                                            .child(
                                                                div()
                                                                    .flex_1()
                                                                    .min_w_0()
                                                                    .child(
                                                                        Input::new(
                                                                            &profile_key_passphrase_input,
                                                                        )
                                                                        .id("favorite-profile-key-passphrase")
                                                                        .w_full(),
                                                                    ),
                                                            )
                                                            .child(
                                                                Button::new(
                                                                    "favorite-key-passphrase-visibility",
                                                                )
                                                                .label(
                                                                    if profile_key_passphrase_visible {
                                                                        "隐藏"
                                                                    } else {
                                                                        "显示"
                                                                    },
                                                                )
                                                                .on_click(
                                                                    move |_, window, cx| {
                                                                        let visible = !favorite_key_passphrase_visibility_draft
                                                                            .read(cx)
                                                                            .profile_key_passphrase_visible;
                                                                        favorite_key_passphrase_visibility_input
                                                                            .update(cx, |input, cx| {
                                                                                input.set_masked(
                                                                                    !visible,
                                                                                    window,
                                                                                    cx,
                                                                                )
                                                                            });
                                                                        favorite_key_passphrase_visibility_draft
                                                                            .update(cx, |draft, cx| {
                                                                                draft.profile_key_passphrase_visible = visible;
                                                                                cx.notify();
                                                                            });
                                                                    },
                                                                ),
                                                            ),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(theme.muted_foreground)
                                                            .child(if creating_profile {
                                                                "加密私钥才需要；会保存到主密码保护的配置中。"
                                                            } else {
                                                                "留空则保留加密配置中的原密钥口令。"
                                                            }),
                                                    ),
                                            ),
                                    )
                                })
                                .when_some(profile_error, |this, error| {
                                    this.child(
                                        div().text_xs().text_color(theme.danger).child(error),
                                    )
                                })
                        }),
                );

            dialog
                .title("设置")
                .w(px(900.))
                .close_button(false)
                .overlay_closable(false)
                .child(
                    v_flex()
                        .relative()
                        .w_full()
                        .h(px(620.))
                        .gap_4()
                        .child(
                            TabBar::new("settings-tabs")
                                .segmented()
                                .selected_index(active_tab.index())
                                .on_click(move |index, _, cx| {
                                    tabs_draft.update(cx, |draft, cx| {
                                        draft.active_tab = if *index == 0 {
                                            SettingsTab::Favorites
                                        } else {
                                            SettingsTab::General
                                        };
                                        draft.error = None;
                                        draft.profile_error = None;
                                        draft.feedback = None;
                                        cx.notify();
                                    });
                                })
                                .child(Tab::new().label("收藏").disabled(saving))
                                .child(Tab::new().label("设置").disabled(saving)),
                        )
                        .when(active_tab == SettingsTab::Favorites, |this| {
                            this.child(favorites)
                        })
                        .when(active_tab == SettingsTab::General, |this| this.child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .min_h_0()
                                .gap_2()
                                .overflow_y_scrollbar()
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
                                        .child("局域网共享"),
                                )
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(div().text_xs().child("发现用户名（可选）"))
                                        .child(
                                            Input::new(&discovery_username_input)
                                                .id("discovery-username")
                                                .w_full(),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(
                                                    "设置后，其他 Blip 的发现列表显示“用户名~IP”；留空时显示本机计算机名。",
                                                ),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(div().text_xs().child("UDP 发现端口"))
                                        .child(
                                            Input::new(&discovery_port_input)
                                                .id("discovery-port")
                                                .w_full(),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format!(
                                                    "局域网内使用 UDP 发现的 Blip 需设置为相同端口；默认 {}，mDNS 不受影响。",
                                                    store::DEFAULT_DISCOVERY_PORT
                                                )),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(div().text_xs().child("HTTP 端口"))
                                        .child(
                                            Input::new(&lan_http_port_input)
                                                .id("lan-http-port")
                                                .w_full(),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format!(
                                                    "浏览器文件共享使用此端口；默认 {}，保存后立即应用。",
                                                    crate::http_share::DEFAULT_PORT
                                                )),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(div().text_xs().child("登录用户名"))
                                        .child(
                                            Input::new(&lan_sftp_username_input)
                                                .id("lan-sftp-username")
                                                .w_full(),
                                        ),
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
                                                        .child("手动密码与设置一起保存在主密码保护的配置中。"),
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
                                        .child("默认共享当前用户主目录；SFTP 与浏览器文件共享共用此目录。"),
                                ),
                        )
                        .when_some(error, |this, error| {
                            this.child(div().text_xs().text_color(theme.danger).child(error))
                        }),
                ))
                        .when_some(feedback, |this, feedback| {
                            let alert = match feedback {
                                SettingsFeedback::Success(message) => {
                                    Alert::success("settings-feedback", message)
                                }
                                SettingsFeedback::Error(message) => {
                                    Alert::error("settings-feedback", message)
                                }
                            };
                            this.child(
                                div()
                                    .absolute()
                                    .top_2()
                                    .right_2()
                                    .w(px(360.))
                                    .child(alert.small().on_close(move |_, _, cx| {
                                        feedback_draft.update(cx, |draft, cx| {
                                            draft.feedback = None;
                                            cx.notify();
                                        });
                                    })),
                            )
                        })
                )
                .footer(
                    if active_tab == SettingsTab::General {
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("settings-cancel")
                                .label("取消")
                                .disabled(saving)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("settings-save")
                                .primary()
                                .label("保存")
                                .loading(saving)
                                .disabled(saving)
                                .on_click(move |_, _, cx| {
                                    save_draft.update(cx, |draft, cx| {
                                        draft.feedback = None;
                                        cx.notify();
                                    });
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
                                    let discovery_username = save_discovery_username_input
                                        .read(cx)
                                        .value()
                                        .trim()
                                        .to_string();
                                    if discovery_username.chars().count()
                                        > crate::discovery::DISCOVERY_USERNAME_MAX_CHARS
                                    {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some(
                                                format!(
                                                    "发现用户名不能超过 {} 个字符",
                                                    crate::discovery::DISCOVERY_USERNAME_MAX_CHARS
                                                )
                                                .into(),
                                            );
                                            cx.notify();
                                        });
                                        return;
                                    }
                                    let discovery_username =
                                        (!discovery_username.is_empty()).then_some(discovery_username);
                                    let Some(discovery_port) = parse_port(
                                        save_discovery_port_input.read(cx).value().as_ref(),
                                    ) else {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some(
                                                "发现端口必须是 1–65535 之间的整数".into(),
                                            );
                                            cx.notify();
                                        });
                                        return;
                                    };
                                    let Some(lan_http_port) = parse_port(
                                        save_lan_http_port_input.read(cx).value().as_ref(),
                                    ) else {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some(
                                                "HTTP 端口必须是 1–65535 之间的整数".into(),
                                            );
                                            cx.notify();
                                        });
                                        return;
                                    };
                                    let lan_sftp_username = save_lan_sftp_username_input
                                        .read(cx)
                                        .value()
                                        .trim()
                                        .to_string();
                                    if lan_sftp_username.is_empty()
                                        || lan_sftp_username.chars().any(char::is_whitespace)
                                    {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some(
                                                if lan_sftp_username.is_empty() {
                                                    "登录用户名不能为空"
                                                } else {
                                                    "登录用户名不能包含空白字符"
                                                }
                                                .into(),
                                            );
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
                                            Some(password)
                                        } else {
                                            None
                                        };
                                    let settings = AppSettings {
                                        remote_open_behavior: behavior,
                                        editor_path,
                                        discovery_username,
                                        discovery_port,
                                        lan_sftp_root,
                                        lan_sftp_username,
                                        lan_sftp_password_mode,
                                        lan_http_port,
                                    };
                                    save_draft.update(cx, |draft, cx| {
                                        draft.saving = true;
                                        draft.error = None;
                                        draft.feedback = None;
                                        cx.notify();
                                    });
                                    let task_draft = save_draft.clone();
                                    let task_app = save_app.clone();
                                    cx.spawn(async move |cx| {
                                        let persisted_settings = settings.clone();
                                        let persisted_password = manual_password.clone();
                                        let result = cx
                                            .background_executor()
                                            .spawn(async move {
                                                store::save_settings_with_lan_password(
                                                    &persisted_settings,
                                                    persisted_password.as_deref(),
                                                )
                                            })
                                            .await;
                                        if let Err(message) = result {
                                            task_draft.update(cx, |draft, cx| {
                                                draft.saving = false;
                                                draft.feedback = None;
                                                draft.error =
                                                    Some(SharedString::from(message));
                                                cx.notify();
                                            });
                                            return;
                                        }

                                        let update_result = task_app.update(cx, |app, cx| {
                                            app.lan_server = match &app.lan_server {
                                                Ok(server) => server
                                                    .set_username(&settings.lan_sftp_username)
                                                    .and_then(|_| {
                                                        server.set_root(effective_root.clone())
                                                    })
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
                                                    &settings.lan_sftp_username,
                                                    manual_password.clone(),
                                                    app.lan_transfer_activity.clone(),
                                                )
                                                .map_err(SharedString::from),
                                            };
                                            let mut apply_errors = Vec::new();
                                            if let Err(message) = &app.lan_server {
                                                apply_errors.push(format!("SFTP：{message}"));
                                            }
                                            if let Err(message) = app.apply_http_settings(
                                                &settings,
                                                manual_password.clone(),
                                                effective_root.clone(),
                                            ) {
                                                apply_errors.push(format!("HTTP：{message}"));
                                            }
                                            app.settings = settings.clone();
                                            if let Some(discovery) = &app._discovery {
                                                if let Err(message) = discovery
                                                    .set_discovery_port(settings.discovery_port)
                                                {
                                                    log::warn!(
                                                        "更新 UDP 设备发现端口失败：{message}"
                                                    );
                                                    apply_errors
                                                        .push(format!("设备发现：{message}"));
                                                }
                                                if let Err(message) = discovery.set_username(
                                                    settings.discovery_username.as_deref(),
                                                ) {
                                                    log::warn!(
                                                        "更新设备发现用户名失败：{message}"
                                                    );
                                                }
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
                                            (!apply_errors.is_empty()).then(|| {
                                                SharedString::from(apply_errors.join("；"))
                                            })
                                        });
                                        let feedback = match update_result {
                                            Ok(Some(message)) => SettingsFeedback::Error(
                                                format!(
                                                    "设置已保存，但部分设置未生效：{message}"
                                                )
                                                .into(),
                                            ),
                                            Ok(None) => SettingsFeedback::Success(
                                                "设置已保存".into(),
                                            ),
                                            Err(error) => SettingsFeedback::Error(
                                                format!("设置已保存，但应用失败：{error}").into(),
                                            ),
                                        };
                                        task_draft.update(cx, |draft, cx| {
                                            draft.saving = false;
                                            draft.error = None;
                                            draft.feedback = Some(feedback);
                                            cx.notify();
                                        });
                                    })
                                    .detach();
                                }),
                        )
                        .into_any_element()
                    } else if creating_profile {
                        let cancel_draft = draft.clone();
                        let add_toolbar = toolbar.clone();
                        let add_draft = draft.clone();
                        let add_name_input = profile_name_input.clone();
                        let add_host_input = profile_host_input.clone();
                        let add_port_input = profile_port_input.clone();
                        let add_username_input = profile_username_input.clone();
                        let add_key_path_input = profile_key_path_input.clone();
                        let add_password_input = profile_password_input.clone();
                        let add_key_passphrase_input = profile_key_passphrase_input.clone();

                        h_flex()
                            .w_full()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("favorite-add-cancel")
                                    .label("取消")
                                    .on_click(move |_, _, cx| {
                                        cancel_draft.update(cx, |draft, cx| {
                                            draft.selected_profile = None;
                                            draft.creating_profile = false;
                                            draft.profile_error = None;
                                            draft.feedback = None;
                                            cx.notify();
                                        });
                                    }),
                            )
                            .child(
                                Button::new("favorite-add-save")
                                    .primary()
                                    .label("添加")
                                    .on_click(move |_, window, cx| {
                                        add_draft.update(cx, |draft, cx| {
                                            draft.feedback = None;
                                            cx.notify();
                                        });
                                        let use_key = add_draft.read(cx).profile_use_key;
                                        let name = add_name_input.read(cx).value().to_string();
                                        let host = add_host_input.read(cx).value().to_string();
                                        let port = add_port_input.read(cx).value().to_string();
                                        let username =
                                            add_username_input.read(cx).value().to_string();
                                        let key_path =
                                            add_key_path_input.read(cx).value().to_string();
                                        let profile = match new_favorite_profile(
                                            &name,
                                            &host,
                                            &port,
                                            &username,
                                            use_key,
                                            &key_path,
                                        ) {
                                            Ok(profile) => profile,
                                            Err(message) => {
                                                add_draft.update(cx, |draft, cx| {
                                                    draft.profile_error = Some(message.clone());
                                                    cx.notify();
                                                });
                                                return;
                                            }
                                        };
                                        let existing_profiles =
                                            add_toolbar.read(cx).profiles_for_display();
                                        if existing_profiles.iter().any(|existing| {
                                            existing.host == profile.host
                                                && existing.port == profile.port
                                                && existing.username == profile.username
                                        }) {
                                            add_draft.update(cx, |draft, cx| {
                                                draft.profile_error = Some(
                                                    "该主机、端口和用户名已在收藏中，请直接编辑原条目"
                                                        .into(),
                                                );
                                                cx.notify();
                                            });
                                            return;
                                        }
                                        if existing_profiles.len() >= store::PROFILE_LIMIT
                                            && existing_profiles.iter().all(|profile| profile.pinned)
                                        {
                                            add_draft.update(cx, |draft, cx| {
                                                draft.profile_error = Some(
                                                    "收藏已满且全部置顶，请先取消一个置顶或删除收藏"
                                                        .into(),
                                                );
                                                cx.notify();
                                            });
                                            return;
                                        }

                                        let password = (!use_key).then(|| {
                                            add_password_input.read(cx).value().to_string()
                                        });
                                        if password.as_ref().is_some_and(String::is_empty) {
                                            add_draft.update(cx, |draft, cx| {
                                                draft.profile_error = Some("请填写密码".into());
                                                cx.notify();
                                            });
                                            return;
                                        }
                                        let key_passphrase = use_key
                                            .then(|| {
                                                add_key_passphrase_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string()
                                            })
                                            .filter(|passphrase| !passphrase.is_empty());
                                        let password_config = profile_password_config(&profile);
                                        if let Some(password) = password.as_deref()
                                            && let Err(message) =
                                                store::store_password(&password_config, password)
                                        {
                                            add_draft.update(cx, |draft, cx| {
                                                draft.profile_error =
                                                    Some(SharedString::from(message));
                                                cx.notify();
                                            });
                                            return;
                                        }
                                        if let Some(passphrase) = key_passphrase.as_deref()
                                            && let Err(message) = store::store_key_passphrase(
                                                &password_config,
                                                passphrase,
                                            )
                                        {
                                            add_draft.update(cx, |draft, cx| {
                                                draft.profile_error =
                                                    Some(SharedString::from(message));
                                                cx.notify();
                                            });
                                            return;
                                        }

                                        match add_toolbar.update(cx, |toolbar, cx| {
                                            toolbar.commit_profile(
                                                profile.clone(),
                                                None,
                                                false,
                                                window,
                                                cx,
                                            )
                                        }) {
                                            Ok(()) => {
                                                let profiles = add_toolbar
                                                    .read(cx)
                                                    .profiles_for_display();
                                                add_password_input.update(cx, |input, cx| {
                                                    input.set_value(String::new(), window, cx);
                                                    input.set_masked(true, window, cx);
                                                    input.set_placeholder(
                                                        "留空则保留原密码",
                                                        window,
                                                        cx,
                                                    );
                                                });
                                                add_key_passphrase_input.update(
                                                    cx,
                                                    |input, cx| {
                                                        input.set_value(
                                                            String::new(),
                                                            window,
                                                            cx,
                                                        );
                                                        input.set_masked(true, window, cx);
                                                        input.set_placeholder(
                                                            "留空则保留原密钥口令",
                                                            window,
                                                            cx,
                                                        );
                                                    },
                                                );
                                                add_draft.update(cx, |draft, cx| {
                                                    draft.profiles = profiles;
                                                    draft.selected_profile =
                                                        Some(profile.clone());
                                                    draft.creating_profile = false;
                                                    draft.profile_password_visible = false;
                                                    draft.profile_key_passphrase_visible = false;
                                                    draft.profile_error = None;
                                                    draft.feedback = Some(
                                                        SettingsFeedback::Success(
                                                            "收藏已添加".into(),
                                                        ),
                                                    );
                                                    cx.notify();
                                                });
                                            }
                                            Err(message) => {
                                                if password.is_some() {
                                                    store::forget_password(&password_config);
                                                }
                                                if key_passphrase.is_some() {
                                                    store::forget_key_passphrase(&password_config);
                                                }
                                                add_draft.update(cx, |draft, cx| {
                                                    draft.feedback = None;
                                                    draft.profile_error =
                                                        Some(SharedString::from(message));
                                                    cx.notify();
                                                });
                                            }
                                        }
                                    }),
                            )
                            .into_any_element()
                    } else if let Some(profile) = selected_profile.clone() {
                        let pin_toolbar = toolbar.clone();
                        let pin_draft = draft.clone();
                        let pin_profile = profile.clone();
                        let pinned = profile.pinned;
                        let delete_toolbar = toolbar.clone();
                        let delete_draft = draft.clone();
                        let delete_profile = profile.clone();
                        let connect_toolbar = toolbar.clone();
                        let connect_profile = profile.clone();
                        let save_toolbar = toolbar.clone();
                        let save_profile = profile.clone();
                        let save_profile_draft = draft.clone();
                        let save_name_input = profile_name_input.clone();
                        let save_host_input = profile_host_input.clone();
                        let save_port_input = profile_port_input.clone();
                        let save_username_input = profile_username_input.clone();
                        let save_key_path_input = profile_key_path_input.clone();
                        let save_profile_password_input = profile_password_input.clone();
                        let save_key_passphrase_input = profile_key_passphrase_input.clone();

                        h_flex()
                            .w_full()
                            .justify_between()
                            .gap_2()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("favorite-delete")
                                            .danger()
                                            .label("删除")
                                            .on_click(move |_, _, cx| {
                                                match delete_toolbar.update(
                                                    cx,
                                                    |toolbar, cx| {
                                                        toolbar.remove_profile(
                                                            &delete_profile,
                                                            cx,
                                                        )
                                                    },
                                                ) {
                                                    Ok(()) => {
                                                        let profiles = delete_toolbar
                                                            .read(cx)
                                                            .profiles_for_display();
                                                        delete_draft.update(
                                                            cx,
                                                            |draft, cx| {
                                                                draft.profiles = profiles;
                                                                draft.selected_profile = None;
                                                                draft.creating_profile = false;
                                                                draft.profile_error = None;
                                                                draft.feedback = Some(
                                                                    SettingsFeedback::Success(
                                                                        "收藏及其凭据已删除".into(),
                                                                    ),
                                                                );
                                                                cx.notify();
                                                            },
                                                        );
                                                    }
                                                    Err(message) => delete_draft.update(
                                                        cx,
                                                        |draft, cx| {
                                                            draft.feedback = None;
                                                            draft.profile_error = Some(
                                                                SharedString::from(message),
                                                            );
                                                            cx.notify();
                                                        },
                                                    ),
                                                }
                                            }),
                                    )
                                    .child(
                                        Button::new("favorite-pin")
                                            .label(if pinned { "取消置顶" } else { "置顶" })
                                            .on_click(move |_, window, cx| {
                                                match pin_toolbar.update(cx, |toolbar, cx| {
                                                    toolbar.set_profile_pinned(
                                                        pin_profile.clone(),
                                                        !pinned,
                                                        window,
                                                        cx,
                                                    )
                                                }) {
                                                    Ok(()) => {
                                                        let mut selected = pin_profile.clone();
                                                        selected.pinned = !pinned;
                                                        let profiles = pin_toolbar
                                                            .read(cx)
                                                            .profiles_for_display();
                                                        pin_draft.update(cx, |draft, cx| {
                                                            draft.profiles = profiles;
                                                            draft.selected_profile =
                                                                Some(selected.clone());
                                                            draft.profile_error = None;
                                                            cx.notify();
                                                        });
                                                    }
                                                    Err(message) => pin_draft.update(
                                                        cx,
                                                        |draft, cx| {
                                                            draft.profile_error = Some(
                                                                SharedString::from(message),
                                                            );
                                                            cx.notify();
                                                        },
                                                    ),
                                                }
                                            }),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("favorite-close")
                                            .label("关闭")
                                            .on_click(|_, window, cx| window.close_dialog(cx)),
                                    )
                                    .child(
                                        Button::new("favorite-connect")
                                            .label("连接")
                                            .on_click(move |_, window, cx| {
                                                connect_toolbar.update(cx, |toolbar, cx| {
                                                    toolbar.connect_profile(
                                                        &connect_profile,
                                                        window,
                                                        cx,
                                                    )
                                                });
                                                window.close_dialog(cx);
                                            }),
                                    )
                                    .child(
                                        Button::new("favorite-save")
                                            .primary()
                                            .label("保存")
                                            .on_click(move |_, window, cx| {
                                                save_profile_draft.update(
                                                    cx,
                                                    |draft, cx| {
                                                        draft.feedback = None;
                                                        cx.notify();
                                                    },
                                                );
                                                let use_key = save_profile_draft
                                                    .read(cx)
                                                    .profile_use_key;
                                                let mut edited = match new_favorite_profile(
                                                    save_name_input.read(cx).value().as_ref(),
                                                    save_host_input.read(cx).value().as_ref(),
                                                    save_port_input.read(cx).value().as_ref(),
                                                    save_username_input.read(cx).value().as_ref(),
                                                    use_key,
                                                    save_key_path_input.read(cx).value().as_ref(),
                                                ) {
                                                    Ok(profile) => profile,
                                                    Err(message) => {
                                                        save_profile_draft.update(
                                                            cx,
                                                            |draft, cx| {
                                                                draft.profile_error =
                                                                    Some(message.clone());
                                                                cx.notify();
                                                            },
                                                        );
                                                        return;
                                                    }
                                                };
                                                edited.pinned = save_profile.pinned;
                                                edited.last_connected = save_profile.last_connected;
                                                let identity_changed =
                                                    edited.host != save_profile.host
                                                        || edited.port != save_profile.port
                                                        || edited.username
                                                            != save_profile.username;
                                                if identity_changed
                                                    && save_toolbar
                                                        .read(cx)
                                                        .profiles_for_display()
                                                        .iter()
                                                        .any(|profile| {
                                                            profile.host == edited.host
                                                                && profile.port == edited.port
                                                                && profile.username
                                                                    == edited.username
                                                        })
                                                {
                                                    save_profile_draft.update(
                                                        cx,
                                                        |draft, cx| {
                                                            draft.profile_error = Some(
                                                                "该主机、端口和用户名已在收藏中"
                                                                    .into(),
                                                            );
                                                            cx.notify();
                                                        },
                                                    );
                                                    return;
                                                }
                                                let password = save_profile_password_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let new_password = match favorite_password_update(
                                                    &save_profile.auth,
                                                    use_key,
                                                    &password,
                                                ) {
                                                    Ok(password) => password,
                                                    Err(message) => {
                                                        save_profile_draft.update(
                                                            cx,
                                                            |draft, cx| {
                                                                draft.profile_error =
                                                                    Some(message.clone());
                                                                cx.notify();
                                                            },
                                                        );
                                                        return;
                                                    }
                                                };
                                                let new_key_passphrase = use_key
                                                    .then(|| {
                                                        save_key_passphrase_input
                                                            .read(cx)
                                                            .value()
                                                            .to_string()
                                                    })
                                                    .filter(|passphrase| !passphrase.is_empty());
                                                let password_config =
                                                    profile_password_config(&edited);
                                                if let Some(password) = new_password.as_deref()
                                                    && let Err(message) = store::store_password(
                                                        &password_config,
                                                        password,
                                                    )
                                                {
                                                    save_profile_draft.update(
                                                        cx,
                                                        |draft, cx| {
                                                            draft.profile_error = Some(
                                                                SharedString::from(message),
                                                            );
                                                            cx.notify();
                                                        },
                                                    );
                                                    return;
                                                }
                                                if let Some(passphrase) =
                                                    new_key_passphrase.as_deref()
                                                    && let Err(message) =
                                                        store::store_key_passphrase(
                                                            &password_config,
                                                            passphrase,
                                                        )
                                                {
                                                    save_profile_draft.update(
                                                        cx,
                                                        |draft, cx| {
                                                            draft.profile_error = Some(
                                                                SharedString::from(message),
                                                            );
                                                            cx.notify();
                                                        },
                                                    );
                                                    return;
                                                }

                                                match save_toolbar.update(cx, |toolbar, cx| {
                                                    toolbar.commit_profile(
                                                        edited.clone(),
                                                        Some(&save_profile),
                                                        new_password.is_none()
                                                            && new_key_passphrase.is_none(),
                                                        window,
                                                        cx,
                                                    )
                                                }) {
                                                    Ok(()) => {
                                                        let profiles = save_toolbar
                                                            .read(cx)
                                                            .profiles_for_display();
                                                        save_profile_password_input.update(
                                                            cx,
                                                            |input, cx| {
                                                                input.set_value(
                                                                    String::new(),
                                                                    window,
                                                                    cx,
                                                                );
                                                                input.set_masked(true, window, cx);
                                                            },
                                                        );
                                                        save_key_passphrase_input.update(
                                                            cx,
                                                            |input, cx| {
                                                                input.set_value(
                                                                    String::new(),
                                                                    window,
                                                                    cx,
                                                                );
                                                                input.set_masked(true, window, cx);
                                                            },
                                                        );
                                                        save_profile_draft.update(
                                                            cx,
                                                            |draft, cx| {
                                                                draft.profiles = profiles;
                                                                draft.selected_profile =
                                                                    Some(edited.clone());
                                                                draft.profile_password_visible =
                                                                    false;
                                                                draft.profile_key_passphrase_visible =
                                                                    false;
                                                                draft.profile_error = None;
                                                                draft.feedback = Some(
                                                                    SettingsFeedback::Success(
                                                                        "收藏已保存".into(),
                                                                    ),
                                                                );
                                                                cx.notify();
                                                            },
                                                        );
                                                    }
                                                    Err(message) => save_profile_draft.update(
                                                        cx,
                                                        |draft, cx| {
                                                            draft.feedback = None;
                                                            draft.profile_error = Some(
                                                                SharedString::from(message),
                                                            );
                                                            cx.notify();
                                                        },
                                                    ),
                                                }
                                            }),
                                    ),
                            )
                            .into_any_element()
                    } else {
                        h_flex()
                            .w_full()
                            .justify_end()
                            .child(
                                Button::new("favorites-close")
                                    .label("关闭")
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .into_any_element()
                    },
                )
        });
    }

    /// 演示模式：把内置服务器的地址填进表单并直接连上。
    ///
    /// 只在 `--features demo-server` 下编译。故意 `remember = false` ——
    /// 别把假账号写进用户的加密配置。
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
            .child(match &self.http_server {
                Ok(server) => Button::new("http-share-status")
                    .link()
                    .xsmall()
                    .compact()
                    .label(SharedString::from(format!("HTTP · {}", server.endpoint())))
                    .tooltip("查看浏览器文件共享信息")
                    .on_click(
                        cx.listener(|app, _, window, cx| app.open_http_server_dialog(window, cx)),
                    )
                    .into_any_element(),
                Err(_) => Button::new("http-share-status")
                    .link()
                    .xsmall()
                    .compact()
                    .label("HTTP · 启动失败")
                    .tooltip("查看浏览器文件共享错误")
                    .on_click(
                        cx.listener(|app, _, window, cx| app.open_http_server_dialog(window, cx)),
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

        let root = v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground);

        #[cfg(target_os = "linux")]
        let root = root.child(
            gpui_kit::component::TitleBar::new().child(
                h_flex()
                    .h_full()
                    .gap_2()
                    .child(img(crate::ui::app_icon::image()).size(px(18.)))
                    .child(
                        div()
                            .text_sm()
                            .child(SharedString::from(self.window_title(cx))),
                    ),
            ),
        );

        root.child(self.toolbar.clone())
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
    fn http_share_qr_code_is_a_square_png() {
        let (qr_code, size) = qr_code_image("http://192.168.1.10:1986").unwrap();
        let raster = ::image::load_from_memory(&qr_code.bytes).unwrap();
        let pixels = raster.to_luma8();

        assert_eq!(raster.width(), raster.height());
        assert_eq!(size, raster.width() as f32);
        assert!(raster.width() >= 150);
        assert!(pixels.pixels().any(|pixel| pixel.0 == [0]));
        assert!(pixels.pixels().any(|pixel| pixel.0 == [255]));
    }

    #[test]
    fn favorite_password_edit_preserves_or_requires_credentials_as_needed() {
        assert_eq!(
            favorite_password_update(&SavedAuth::Password, false, "").unwrap(),
            None
        );
        assert_eq!(
            favorite_password_update(&SavedAuth::Password, false, " new secret ").unwrap(),
            Some(" new secret ".to_string())
        );
        assert!(
            favorite_password_update(
                &SavedAuth::Key {
                    path: "/tmp/id_ed25519".to_string(),
                },
                false,
                "",
            )
            .is_err()
        );
        assert_eq!(
            favorite_password_update(&SavedAuth::Password, true, "ignored").unwrap(),
            None
        );
    }

    #[test]
    fn manual_favorite_requires_connection_identity_and_builds_auth() {
        assert!(new_favorite_profile("", "", "22", "root", false, "").is_err());
        assert!(new_favorite_profile("", "server", "0", "root", false, "").is_err());
        assert!(new_favorite_profile("", "server", "22", "", false, "").is_err());

        let profile = new_favorite_profile(
            " Work ",
            " server.example ",
            " 2022 ",
            " alice ",
            true,
            "/tmp/id_ed25519",
        )
        .unwrap();
        assert_eq!(profile.name.as_deref(), Some("Work"));
        assert_eq!(profile.host, "server.example");
        assert_eq!(profile.port, 2022);
        assert_eq!(profile.username, "alice");
        assert_eq!(profile.last_connected, None);
        assert!(matches!(
            profile.auth,
            SavedAuth::Key { ref path } if path == "/tmp/id_ed25519"
        ));
    }

    #[test]
    fn port_parser_accepts_only_the_valid_tcp_udp_range() {
        assert_eq!(parse_port(" 1 "), Some(1));
        assert_eq!(parse_port("65535"), Some(65535));
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port("65536"), None);
        assert_eq!(parse_port("not-a-port"), None);
    }

    #[test]
    fn favorite_tooltip_shows_complete_connection_data() {
        let profile = new_favorite_profile(
            "Work",
            "server.example",
            "2022",
            "alice",
            true,
            "/tmp/id_ed25519",
        )
        .unwrap();

        assert_eq!(
            favorite_tooltip(&profile),
            "名称：Work\n登录名：alice\n主机/IP：server.example\n端口：2022\n认证：私钥\n私钥：/tmp/id_ed25519\n\n双击连接"
        );
    }

    #[test]
    fn favorite_persists_the_password_that_was_actually_used() {
        let config = SessionConfig {
            host: "server.example".to_string(),
            port: 2222,
            username: "alice".to_string(),
            // 这可以是 build_config 从加密配置补出来的密码；密码输入框仍可能为空。
            auth: AuthMethod::Password("resolved-from-config".to_string()),
        };

        let prepared = prepare_profile_persistence(&config);

        assert_eq!(prepared.password.as_deref(), Some("resolved-from-config"));
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
