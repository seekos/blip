//! 顶部工具栏：只有连接表单（Host / Port / Username / Password）+ 连接状态。
//!
//! 文件操作（上传 / 下载 / 新建目录 / 删除 / 刷新）不再占一行按钮，全部收进
//! 两个文件面板的右键菜单 —— 见 `file_pane::menu_spec`。
//!
//! 工具栏自己持有输入状态，只把「用户想干什么」以 [`ToolbarEvent`] 抛给根实体，
//! 不直接碰 worker、也不碰文件面板 —— 这样连接表单和网络层可以各自演进。

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::sftp::types::{AuthMethod, DEFAULT_SSH_PORT, SessionConfig};
use crate::session::store::{self, SavedAuth, SavedProfile};
use crate::ui::SessionState;

#[derive(Clone, Debug)]
pub enum ToolbarEvent {
    Connect(SessionConfig),
    Disconnect,
}

impl EventEmitter<ToolbarEvent> for Toolbar {}

/// 表单当前值的快照，用于落盘（不含任何明文凭证）。
pub struct ConnectionSnapshot {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub remember: bool,
}

pub struct Toolbar {
    host: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    key_path: Entity<InputState>,
    /// 加密私钥的口令。只在私钥模式下显示，且**从不落盘**。
    key_passphrase: Entity<InputState>,
    use_key: bool,
    remember: bool,
    state: SessionState,
    error: Option<SharedString>,
}

impl Toolbar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            host: cx.new(|cx| InputState::new(window, cx).placeholder("主机名或 IP")),
            port: cx.new(|cx| InputState::new(window, cx).default_value("22")),
            username: cx.new(|cx| {
                // 默认 root：绝大多数场景连的就是 root，省一次输入；
                // 上次成功连接的用户名会由 `prefill` 覆盖掉这个默认值。
                InputState::new(window, cx)
                    .default_value("root")
                    .placeholder("用户名")
            }),
            password: cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("密码")),
            key_path: cx.new(|cx| InputState::new(window, cx).placeholder("留空自动查找 ~/.ssh/id_ed25519")),
            key_passphrase: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("口令（可选）")
            }),
            use_key: false,
            remember: true,
            state: SessionState::Disconnected,
            error: None,
        }
    }

    pub fn set_state(&mut self, state: SessionState, cx: &mut Context<Self>) {
        self.state = state;
        if state != SessionState::Disconnected {
            self.error = None;
        }
        cx.notify();
    }

    pub fn set_error(&mut self, error: Option<SharedString>, cx: &mut Context<Self>) {
        self.error = error;
        cx.notify();
    }

    /// 用上次成功的连接回填表单。
    pub fn prefill(&mut self, profile: &SavedProfile, window: &mut Window, cx: &mut Context<Self>) {
        let host = profile.host.clone();
        let port = profile.port.to_string();
        let username = profile.username.clone();
        let key_path = match &profile.auth {
            SavedAuth::Key { path } => Some(path.clone()),
            SavedAuth::Password => None,
        };

        self.host
            .update(cx, |state, cx| state.set_value(host, window, cx));
        self.port
            .update(cx, |state, cx| state.set_value(port, window, cx));
        self.username
            .update(cx, |state, cx| state.set_value(username, window, cx));
        if let Some(path) = key_path {
            self.use_key = true;
            self.key_path
                .update(cx, |state, cx| state.set_value(path, window, cx));
        }
        cx.notify();
    }

    /// 直接把一组凭证填进表单（演示模式用），不读也不写钥匙串。
    #[cfg(feature = "demo-server")]
    pub fn fill_credentials(
        &mut self,
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        remember: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.use_key = false;
        self.remember = remember;
        self.error = None;
        self.state = SessionState::Disconnected;

        self.host
            .update(cx, |state, cx| state.set_value(host.to_string(), window, cx));
        self.port.update(cx, |state, cx| {
            state.set_value(port.to_string(), window, cx)
        });
        self.username.update(cx, |state, cx| {
            state.set_value(username.to_string(), window, cx)
        });
        self.password.update(cx, |state, cx| {
            state.set_value(password.to_string(), window, cx)
        });
        cx.notify();
    }

    pub fn snapshot(&self, cx: &App) -> ConnectionSnapshot {
        ConnectionSnapshot {
            host: self.host.read(cx).value().trim().to_string(),
            port: self
                .port
                .read(cx)
                .value()
                .trim()
                .parse::<u16>()
                .unwrap_or(DEFAULT_SSH_PORT),
            username: self.username.read(cx).value().trim().to_string(),
            auth: self.current_auth(cx),
            remember: self.remember,
        }
    }

    fn current_auth(&self, cx: &App) -> AuthMethod {
        if self.use_key {
            self.key_auth(cx)
        } else {
            AuthMethod::Password(self.password.read(cx).value().to_string())
        }
    }

    /// 私钥认证配置。口令留空 = `None`（未加密的私钥就不该带口令）。
    fn key_auth(&self, cx: &App) -> AuthMethod {
        let passphrase = self.key_passphrase.read(cx).value().trim().to_string();
        AuthMethod::PrivateKey {
            path: expand_tilde(self.key_path.read(cx).value().trim()),
            passphrase: (!passphrase.is_empty()).then_some(passphrase),
        }
    }

    /// 校验表单并组装连接配置。密码留空时回落到钥匙串里的旧密码。
    pub fn build_config(&self, cx: &App) -> Result<SessionConfig, SharedString> {
        let host = self.host.read(cx).value().trim().to_string();
        if host.is_empty() {
            return Err("请填写主机地址".into());
        }

        let port = self
            .port
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
            .unwrap_or(DEFAULT_SSH_PORT);

        let username = self.username.read(cx).value().trim().to_string();
        if username.is_empty() {
            return Err("请填写用户名".into());
        }

        let auth = if self.use_key {
            // 留空表示「自动查找」：worker 会依次试 ~/.ssh/id_ed25519、id_rsa。
            self.key_auth(cx)
        } else {
            let typed = self.password.read(cx).value().to_string();
            let password = if typed.is_empty() {
                let probe = SessionConfig {
                    host: host.clone(),
                    port,
                    username: username.clone(),
                    auth: AuthMethod::Password(String::new()),
                };
                store::load_password(&probe).unwrap_or_default()
            } else {
                typed
            };
            if password.is_empty() {
                return Err("请填写密码".into());
            }
            AuthMethod::Password(password)
        };

        Ok(SessionConfig {
            host,
            port,
            username,
            auth,
        })
    }

    /// 当前会话状态。窗口标题栏（`ScpApp::sync_window_title`）据此显示
    /// `user@host` —— 连接信息挂在窗口顶部，不再挤进表单行。
    pub fn state(&self) -> SessionState {
        self.state
    }

    fn render_connection_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let connected = self.state == SessionState::Connected;
        let connecting = self.state == SessionState::Connecting;
        let use_key = self.use_key;

        let password_or_key = if use_key {
            Input::new(&self.key_path).id("toolbar-key").w_full()
        } else {
            Input::new(&self.password).id("toolbar-password").w_full()
        };
        h_flex()
            .w_full()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .gap_1p5()
                    .flex_shrink_0()
                    .child(IconName::Terminal)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("macscp"),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(140.))
                            .child(Input::new(&self.host).id("toolbar-host").w_full()),
                    )
                    .child(
                        div()
                            .w(px(72.))
                            .child(Input::new(&self.port).id("toolbar-port").w_full()),
                    )
                    .child(
                        div()
                            .w(px(150.))
                            .child(Input::new(&self.username).id("toolbar-user").w_full()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(150.))
                            .child(password_or_key),
                    )
                    // 私钥模式才出现的口令框：加密私钥没有它根本连不上。
                    .when(use_key, |this| {
                        this.child(
                            div()
                                .w(px(120.))
                                .flex_shrink_0()
                                .child(
                                    Input::new(&self.key_passphrase)
                                        .id("toolbar-passphrase")
                                        .w_full(),
                                ),
                        )
                    })
                    .child(
                        Button::new("toggle-auth-mode")
                            .ghost()
                            .xsmall()
                            .icon(IconName::KeyRound)
                            .tooltip(if use_key {
                                "当前：私钥认证，点击切换为密码"
                            } else {
                                "当前：密码认证，点击切换为私钥"
                            })
                            .disabled(connected)
                            .on_click(cx.listener(|toolbar, _, _, cx| {
                                toolbar.use_key = !toolbar.use_key;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("toggle-remember")
                            .ghost()
                            .xsmall()
                            .icon(if self.remember {
                                IconName::Lock
                            } else {
                                IconName::LockOpen
                            })
                            .tooltip(match (use_key, self.remember) {
                                (true, true) => "记住这台主机（私钥路径会保存，口令不会）",
                                (true, false) => "不保存连接信息",
                                (false, true) => "密码会存入 macOS 钥匙串",
                                (false, false) => "不保存密码",
                            })
                            .disabled(connected)
                            .on_click(cx.listener(|toolbar, _, _, cx| {
                                toolbar.remember = !toolbar.remember;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                Button::new("connect")
                    .when(connected, |this| this.outline())
                    .when(!connected, |this| this.primary())
                    .small()
                    .w(px(92.))
                    .label(match self.state {
                        SessionState::Connected => "断开",
                        SessionState::Connecting => "连接中",
                        SessionState::Disconnected => "连接",
                    })
                    .loading(connecting)
                    .on_click(cx.listener(|toolbar, _, _, cx| match toolbar.state {
                        SessionState::Connected => {
                            // 先切自己的状态再发事件：上层收到事件后就不会再回头
                            // update 本实体（那会撞上 GPUI 的独占租约）。
                            toolbar.state = SessionState::Disconnected;
                            cx.notify();
                            cx.emit(ToolbarEvent::Disconnect);
                        }
                        SessionState::Connecting => {}
                        SessionState::Disconnected => match toolbar.build_config(cx) {
                            Ok(config) => {
                                toolbar.error = None;
                                toolbar.state = SessionState::Connecting;
                                cx.notify();
                                cx.emit(ToolbarEvent::Connect(config));
                            }
                            Err(message) => {
                                toolbar.error = Some(message);
                                cx.notify();
                            }
                        },
                    })),
            )
            // 连接状态（`user@host`）不在这里：它已经挪到窗口标题栏，
            // 见 `ScpApp::sync_window_title`。
            .into_any_element()
    }
}

impl Render for Toolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let connection_row = self.render_connection_row(cx);

        // 错误（表单校验 / 钥匙串 / 连接被拒）只在真出错时多占一行：
        // 常态下工具栏就是一条连接栏，不再有第二条操作栏。
        let error_row = self.error.clone().map(|error| {
            h_flex()
                .w_full()
                .gap_1p5()
                .px_4()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.danger.opacity(0.08))
                .text_xs()
                .text_color(theme.danger)
                .child(IconName::CircleAlert)
                .child(div().flex_1().min_w_0().truncate().child(error))
        });

        v_flex()
            .w_full()
            .flex_shrink_0()
            .child(connection_row)
            .children(error_row)
    }
}

fn expand_tilde(input: &str) -> PathBuf {
    // 从 Finder / 终端复制来的路径常带引号，先剥掉。
    let trimmed = input.trim().trim_matches(['"', '\'']).trim();

    // 单独一个 `~` 也应该是家目录，而不是名为 `~` 的当前目录子项。
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(trimmed));
    }
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}
