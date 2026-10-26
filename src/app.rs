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
    ActiveTheme as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::Notification,
    radio::{Radio, RadioGroup},
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::session::store::{self, AppSettings, RemoteOpenBehavior, SavedAuth, SavedProfile};
use crate::sftp::types::{AuthMethod, SessionConfig};
use crate::ui::SessionState;
use crate::ui::file_pane::FilePane;
use crate::ui::toolbar::{Toolbar, ToolbarEvent};
use crate::ui::transfer_panel::TransferQueue;
use crate::ui::workspace::{Workspace, WorkspaceEvent};

pub struct ScpApp {
    toolbar: Entity<Toolbar>,
    workspace: Entity<Workspace>,
    /// 工作区里两个面板与队列的句柄，状态栏展示路径/任务数时用。
    local: Entity<FilePane>,
    remote: Entity<FilePane>,
    transfers: Entity<TransferQueue>,
    profiles: Vec<SavedProfile>,
    settings: AppSettings,
    /// 当前这次连接实际使用的配置。密码框可能为空（收藏会从钥匙串补密码），
    /// 成功后必须保存这里的认证信息，不能再从空白表单反推。
    pending_config: Option<SessionConfig>,
    _subscriptions: Vec<Subscription>,
}

struct SettingsDraft {
    remote_open_behavior: RemoteOpenBehavior,
    editor_path: Option<PathBuf>,
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
        let toolbar = cx.new(|cx| Toolbar::new(profiles.clone(), window, cx));
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
                        "macscp — 正在连接 {}@{}…",
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
                    window.set_window_title("macscp");
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
                let WorkspaceEvent::SessionStateChanged(state) = event;
                let state = *state;
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

                // 标题栏跟着会话状态走：`macscp — user@host`。
                app.sync_window_title(window, cx);

                // 连接成功才落盘凭证，避免把错误密码写进钥匙串。
                if state == SessionState::Connected {
                    app.persist_profile(cx);
                } else if state == SessionState::Disconnected {
                    // 认证失败、超时或主动断开后，绝不能把这份配置用于下一次成功事件。
                    app.pending_config = None;
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
            pending_config: None,
            _subscriptions: subscriptions,
        };

        app.prefill_last_profile(window, cx);
        app.sync_window_title(window, cx);
        app
    }

    // -----------------------------------------------------------------------
    // 凭证
    // -----------------------------------------------------------------------

    /// 把当前连接写进 Keychain + profiles.json。
    ///
    /// 用户关掉「记住」时**什么都不落盘**：钥匙串里的旧密码顺手清掉，
    /// profiles.json 也不写 —— 否则演示模式（remember = false）会往
    /// 配置里塞一堆 `tester@127.0.0.1:<随机端口>`，下次启动还会被
    /// `prefill` 填回表单。
    fn persist_profile(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.pending_config.take() else {
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
            // 写钥匙串可能触发系统授权弹窗，放到后台线程；失败时回到前台
            // 把原因显示在工具栏上 —— 只写日志等于没告诉用户。
            let this = cx.entity().downgrade();
            cx.spawn(async move |_app, cx| {
                let task = cx
                    .background_executor()
                    .spawn(async move { store::store_password(&password_config, &password) });

                if let Err(err) = task.await {
                    log::warn!("保存密码失败：{err}");
                    let _ = this.update(cx, |app, cx| {
                        let message = SharedString::from(format!("保存密码到钥匙串失败：{err}"));
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

    /// 启动时把最近一次成功的连接填回表单。
    fn prefill_last_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.first().cloned() else {
            return;
        };
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.prefill(&profile, window, cx));
    }

    /// 把连接状态写进窗口标题栏：`macscp — root@192.168.146.7`。
    ///
    /// 这串 `user@host` 原来贴在表单行的最右边，和输入框挤在一起，
    /// 长得就像一个输入框；挪到窗口顶部后，表单那一行也宽松了。
    /// 断开时退回纯 `macscp`。
    fn sync_window_title(&self, window: &mut Window, cx: &App) {
        let toolbar = self.toolbar.read(cx);
        let title = match toolbar.state() {
            SessionState::Connected => {
                let snapshot = toolbar.snapshot(cx);
                format!("macscp — {}@{}", snapshot.username, snapshot.host)
            }
            SessionState::Connecting => "macscp — 正在连接…".to_string(),
            SessionState::Disconnected => "macscp".to_string(),
        };
        window.set_window_title(&title);
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = cx.new(|_| SettingsDraft {
            remote_open_behavior: self.settings.remote_open_behavior,
            editor_path: self.settings.editor_path.clone(),
            error: None,
        });
        let app = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _window, cx| {
            let theme = cx.theme().clone();
            let (selected, editor_path, error) = {
                let snapshot = draft.read(cx);
                (
                    match snapshot.remote_open_behavior {
                        RemoteOpenBehavior::SystemDefault => 0,
                        RemoteOpenBehavior::CustomEditor => 1,
                        RemoteOpenBehavior::BuiltInTextEditor => 2,
                    },
                    snapshot.editor_path.clone(),
                    snapshot.error.clone(),
                )
            };

            let mode_draft = draft.clone();
            let browse_draft = draft.clone();
            let save_draft = draft.clone();
            let save_app = app.clone();

            dialog
                .title("设置")
                .w(px(520.))
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
                                                                    directories: true,
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
                                                                                        SharedString::from("请选择 .app 应用或可执行文件")
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
                                    let (behavior, editor_path) = {
                                        let snapshot = save_draft.read(cx);
                                        (
                                            snapshot.remote_open_behavior,
                                            snapshot.editor_path.clone(),
                                        )
                                    };
                                    if behavior == RemoteOpenBehavior::CustomEditor
                                        && editor_path.as_ref().is_none_or(|path| {
                                            !is_editor_path(path)
                                        })
                                    {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some(SharedString::from(
                                                "请选择 .app 编辑器应用或可执行文件",
                                            ));
                                            cx.notify();
                                        });
                                        return;
                                    }
                                    let settings = AppSettings {
                                        remote_open_behavior: behavior,
                                        editor_path,
                                    };
                                    if let Err(message) = store::save_settings(&settings) {
                                        save_draft.update(cx, |draft, cx| {
                                            draft.error = Some(SharedString::from(message));
                                            cx.notify();
                                        });
                                        return;
                                    }

                                    let _ = save_app.update(cx, |app, cx| {
                                        app.settings = settings.clone();
                                        app.workspace.update(cx, |workspace, cx| {
                                            workspace.set_settings(settings.clone(), cx)
                                        });
                                    });
                                    window.close_dialog(cx);
                                    window.push_notification(
                                        Notification::success("设置已保存"),
                                        cx,
                                    );
                                }),
                        ),
                )
        });
    }

    /// 演示模式：把内置服务器的地址填进表单并直接连上。
    ///
    /// 只在 `--features demo-server` 下编译。故意 `remember = false` ——
    /// 别把假账号写进用户的钥匙串。
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
        let last_error = self.workspace.read(cx).last_error();

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
            .child(
                div()
                    .flex_shrink_0()
                    .child(SharedString::from(if active > 0 {
                        format!("{active} 个传输进行中")
                    } else {
                        "空闲".to_string()
                    })),
            )
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
        || (path.is_dir()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app")))
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
            // 这可以是 build_config 从钥匙串补出来的密码；密码输入框仍可能为空。
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
}
