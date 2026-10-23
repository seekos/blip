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

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::sftp::types::AuthMethod;
use crate::session::store::{self, SavedAuth, SavedProfile};
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
    _subscriptions: Vec<Subscription>,
}

impl ScpApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let toolbar = cx.new(|cx| Toolbar::new(window, cx));
        // 工作区在构造时就启动 SftpWorker 与事件泵。
        let workspace = cx.new(|cx| Workspace::new(window, cx));

        let (local, remote, transfers) = {
            let ws = workspace.read(cx);
            (ws.local(), ws.remote(), ws.transfers())
        };

        let mut subscriptions = Vec::new();

        // 工具栏 → 工作区。只动 workspace，不回头动 toolbar（它正被更新）。
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
                    app.workspace.update(cx, |workspace, cx| workspace.connect(config, cx));
                }
                ToolbarEvent::Disconnect => {
                    app.workspace.update(cx, |workspace, cx| workspace.disconnect(cx));
                    window.set_window_title("macscp");
                }
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
                }
            },
        ));

        let profiles = store::load_profiles();
        let mut app = Self {
            toolbar,
            workspace,
            local,
            remote,
            transfers,
            profiles,
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
        let snapshot = self.toolbar.read(cx).snapshot(cx);
        let auth = match &snapshot.auth {
            AuthMethod::PrivateKey { path, .. } => SavedAuth::Key {
                path: path.to_string_lossy().to_string(),
            },
            AuthMethod::Password(_) => SavedAuth::Password,
        };

        if !snapshot.remember {
            if matches!(snapshot.auth, AuthMethod::Password(_)) {
                let config = crate::sftp::types::SessionConfig {
                    host: snapshot.host.clone(),
                    port: snapshot.port,
                    username: snapshot.username.clone(),
                    auth: AuthMethod::Password(String::new()),
                };
                store::forget_password(&config);
            }
            return;
        }

        if let AuthMethod::Password(password) = &snapshot.auth {
            let password = password.clone();
            let config = crate::sftp::types::SessionConfig {
                host: snapshot.host.clone(),
                port: snapshot.port,
                username: snapshot.username.clone(),
                auth: AuthMethod::Password(password.clone()),
            };
            // 写钥匙串可能触发系统授权弹窗，放到后台线程；失败时回到前台
            // 把原因显示在工具栏上 —— 只写日志等于没告诉用户。
            let this = cx.entity().downgrade();
            cx.spawn(async move |_app, cx| {
                let task = cx
                    .background_executor()
                    .spawn(async move { store::store_password(&config, &password) });

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

        let profile = SavedProfile {
            host: snapshot.host,
            port: snapshot.port,
            username: snapshot.username,
            auth,
        };
        store::upsert_profile(&mut self.profiles, profile);
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
