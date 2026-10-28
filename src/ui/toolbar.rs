//! 顶部工具栏：收藏夹 + 连接表单（Host / Port / Username / Password）+ 连接状态。
//!
//! 文件操作（上传 / 下载 / 新建目录 / 删除 / 刷新）不再占一行按钮，全部收进
//! 两个文件面板的右键菜单 —— 见 `file_pane::menu_spec`。
//!
//! 工具栏自己持有输入状态，只把「用户想干什么」以 [`ToolbarEvent`] 抛给根实体，
//! 不直接碰 worker、也不碰文件面板 —— 这样连接表单和网络层可以各自演进。

use std::path::PathBuf;

#[cfg(target_os = "macos")]
use block2::RcBlock;
use gpui_kit::assets::IconName;
use gpui_kit::base::actions::Cancel;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    notification::Notification,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
#[cfg(target_os = "macos")]
use objc2::{MainThreadMarker, MainThreadOnly};
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSButton, NSControlStateValueOff, NSControlStateValueOn, NSModalResponse, NSModalResponseOK,
    NSObjectNSKeyValueBindingCreation, NSOpenPanel, NSValueBinding, NSView,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURL, ns_string};

use crate::session::store::{self, SavedAuth, SavedProfile};
use crate::sftp::types::{AuthMethod, DEFAULT_SSH_PORT, SessionConfig};
use crate::ui::SessionState;

#[derive(Clone, Debug)]
pub enum ToolbarEvent {
    Connect(SessionConfig),
    Disconnect,
    InstallPublicKey {
        public_key: PathBuf,
        private_key: PathBuf,
    },
    ProfilesChanged(Vec<SavedProfile>),
    OpenSettings,
}

impl EventEmitter<ToolbarEvent> for Toolbar {}

/// 表单当前值的快照，用于窗口标题和“记住连接”开关，不含任何凭证。
pub struct ConnectionSnapshot {
    pub host: String,
    pub username: String,
    pub remember: bool,
}

#[derive(Clone, Copy)]
enum KeyPickerPurpose {
    Private,
    Public,
}

impl KeyPickerPurpose {
    fn prompt(self) -> &'static str {
        match self {
            Self::Private => "选择 SSH 私钥文件",
            Self::Public => "选择要部署的 SSH 公钥文件",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Private => "私钥文件",
            Self::Public => "公钥文件",
        }
    }
}

pub struct Toolbar {
    host: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    /// 用户显式选择的私钥；`None` 时由连接层自动查找默认密钥。
    key_path: Option<PathBuf>,
    /// 下次打开私钥选择器时是否显示隐藏文件；也会跟随选择器内的开关更新。
    show_hidden_key_files: bool,
    choosing_key: bool,
    deploying_key: bool,
    /// 加密私钥的口令。只在私钥模式下显示，且**从不落盘**。
    key_passphrase: Entity<InputState>,
    profiles: Vec<SavedProfile>,
    use_key: bool,
    remember: bool,
    state: SessionState,
    error: Option<SharedString>,
}

impl Toolbar {
    pub fn new(profiles: Vec<SavedProfile>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            key_path: None,
            show_hidden_key_files: false,
            choosing_key: false,
            deploying_key: false,
            key_passphrase: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("口令（可选）")
            }),
            profiles,
            use_key: false,
            remember: true,
            state: SessionState::Disconnected,
            error: None,
        }
    }

    pub fn set_state(&mut self, state: SessionState, cx: &mut Context<Self>) {
        self.state = state;
        if state != SessionState::Connected {
            self.choosing_key = false;
            self.deploying_key = false;
        }
        if state != SessionState::Disconnected {
            self.error = None;
        }
        cx.notify();
    }

    pub fn finish_public_key_install(
        &mut self,
        result: Result<PathBuf, SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.choosing_key = false;
        self.deploying_key = false;
        match result {
            Ok(private_key) => {
                self.use_key = true;
                self.key_path = Some(private_key);
                self.error = None;
            }
            Err(message) => self.error = Some(message),
        }
        cx.notify();
    }

    fn pick_key(&mut self, purpose: KeyPickerPurpose, cx: &mut Context<Self>) {
        if self.choosing_key || self.deploying_key {
            return;
        }
        self.choosing_key = true;
        self.error = None;
        let show_hidden_files = self.show_hidden_key_files;
        let initial_directory = private_key_initial_directory(match purpose {
            KeyPickerPurpose::Private => self.key_path.as_deref(),
            KeyPickerPurpose::Public => None,
        });
        cx.notify();

        #[cfg(target_os = "macos")]
        let receiver = prompt_for_key_file(
            purpose.prompt(),
            show_hidden_files,
            initial_directory.as_deref(),
        );
        #[cfg(not(target_os = "macos"))]
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(purpose.prompt().into()),
        });

        let entity = cx.entity().downgrade();
        #[cfg(target_os = "macos")]
        cx.spawn(async move |_this, cx| match receiver.recv_async().await {
            Ok(Ok(selection)) => {
                let _ = entity.update(cx, |toolbar, cx| {
                    toolbar.show_hidden_key_files = selection.show_hidden_files;
                    toolbar.accept_key_selection(purpose, selection.path, cx);
                });
            }
            Ok(Err(message)) => {
                let _ = entity.update(cx, |toolbar, cx| toolbar.fail_key_selection(message, cx));
            }
            Err(_) => {
                let _ = entity.update(cx, |toolbar, cx| {
                    toolbar.fail_key_selection(
                        format!("{}选择器意外关闭，请重试", purpose.label()),
                        cx,
                    )
                });
            }
        })
        .detach();

        #[cfg(not(target_os = "macos"))]
        cx.spawn(async move |_this, cx| match receiver.await {
            Ok(Ok(paths)) => {
                let path = paths.and_then(|paths| paths.into_iter().next());
                let _ = entity.update(cx, |toolbar, cx| {
                    toolbar.accept_key_selection(purpose, path, cx)
                });
            }
            Ok(Err(err)) => {
                let _ = entity.update(cx, |toolbar, cx| {
                    toolbar.fail_key_selection(format!("无法选择{}：{err}", purpose.label()), cx)
                });
            }
            Err(_) => {
                let _ = entity.update(cx, |toolbar, cx| {
                    toolbar.fail_key_selection(
                        format!("{}选择器意外关闭，请重试", purpose.label()),
                        cx,
                    )
                });
            }
        })
        .detach();
    }

    fn accept_key_selection(
        &mut self,
        purpose: KeyPickerPurpose,
        path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.choosing_key = false;
        let Some(path) = path else {
            cx.notify();
            return;
        };
        match purpose {
            KeyPickerPurpose::Private => {
                self.key_path = Some(path);
                self.error = None;
            }
            KeyPickerPurpose::Public => {
                if self.state != SessionState::Connected {
                    self.error = Some("连接已断开，请重新连接后部署公钥".into());
                } else {
                    match deployment_key_pair(&path) {
                        Ok(private_key) => {
                            self.deploying_key = true;
                            cx.emit(ToolbarEvent::InstallPublicKey {
                                public_key: path,
                                private_key,
                            });
                        }
                        Err(message) => self.error = Some(message.into()),
                    }
                }
            }
        }
        cx.notify();
    }

    fn fail_key_selection(&mut self, message: String, cx: &mut Context<Self>) {
        self.choosing_key = false;
        self.error = Some(message.into());
        cx.notify();
    }

    pub fn set_error(&mut self, error: Option<SharedString>, cx: &mut Context<Self>) {
        self.error = error;
        cx.notify();
    }

    pub fn set_profiles(&mut self, profiles: Vec<SavedProfile>, cx: &mut Context<Self>) {
        self.profiles = profiles;
        cx.notify();
    }

    /// 用上次成功的连接回填表单。
    pub fn prefill(&mut self, profile: &SavedProfile, window: &mut Window, cx: &mut Context<Self>) {
        let host = profile.host.clone();
        let port = profile.port.to_string();
        let username = profile.username.clone();
        self.use_key = matches!(profile.auth, SavedAuth::Key { .. });
        self.remember = true;
        self.error = None;

        self.host
            .update(cx, |state, cx| state.set_value(host, window, cx));
        self.port
            .update(cx, |state, cx| state.set_value(port, window, cx));
        self.username
            .update(cx, |state, cx| state.set_value(username, window, cx));
        self.password
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        self.key_passphrase
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        self.key_path = match &profile.auth {
            SavedAuth::Key { path } if !path.trim().is_empty() => Some(PathBuf::from(path)),
            _ => None,
        };
        cx.notify();
    }

    fn connect_profile(
        &mut self,
        profile: &SavedProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state == SessionState::Connecting {
            return;
        }
        self.prefill(profile, window, cx);
        self.start_connect(cx);
    }

    fn start_connect(&mut self, cx: &mut Context<Self>) {
        match self.build_config(cx) {
            Ok(config) => {
                self.error = None;
                self.state = SessionState::Connecting;
                cx.notify();
                cx.emit(ToolbarEvent::Connect(config));
            }
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    fn remove_profile(
        &mut self,
        profile: &SavedProfile,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let mut profiles = self.profiles.clone();
        let Some(removed) = store::remove_profile(&mut profiles, &profile.username, &profile.host)
        else {
            return Ok(());
        };
        store::save_profiles(&profiles)?;

        if matches!(removed.auth, SavedAuth::Password) {
            let config = SessionConfig {
                host: removed.host,
                port: removed.port,
                username: removed.username,
                auth: AuthMethod::Password(String::new()),
            };
            store::forget_password(&config);
        }

        self.profiles = profiles.clone();
        cx.notify();
        cx.emit(ToolbarEvent::ProfilesChanged(profiles));
        Ok(())
    }

    /// 直接把一组凭证填进表单（演示模式用），不读也不写钥匙串。
    #[cfg(feature = "demo-server")]
    #[allow(clippy::too_many_arguments)]
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

        self.host.update(cx, |state, cx| {
            state.set_value(host.to_string(), window, cx)
        });
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
            username: self.username.read(cx).value().trim().to_string(),
            remember: self.remember,
        }
    }

    /// 私钥认证配置。口令留空 = `None`（未加密的私钥就不该带口令）。
    fn key_auth(&self, cx: &App) -> AuthMethod {
        let passphrase = self.key_passphrase.read(cx).value().trim().to_string();
        AuthMethod::PrivateKey {
            path: self
                .key_path
                .as_ref()
                .map(|path| expand_tilde(&path.to_string_lossy()))
                .unwrap_or_default(),
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
                match store::load_password(&probe) {
                    Ok(Some(password)) if password.is_empty() => {
                        return Err(
                            "这个收藏的旧密码记录为空，请重新输入一次；连接成功后会自动修复".into(),
                        );
                    }
                    Ok(Some(password)) => password,
                    Ok(None) => String::new(),
                    Err(message) => return Err(message.into()),
                }
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
        let form_locked = connected || connecting;
        let use_key = self.use_key;
        let profiles = self.profiles.clone();
        let toolbar = cx.entity().downgrade();

        let password_or_key = if use_key {
            let choosing_key = self.choosing_key;
            let selected_path = self
                .key_path
                .as_ref()
                .map(|path| path.to_string_lossy().to_string());
            let key_label = selected_path
                .clone()
                .unwrap_or_else(|| "选择私钥文件…".to_string());
            let key_tooltip = selected_path.clone().unwrap_or_else(|| {
                "选择私钥文件；不选择时自动查找 ~/.ssh/id_ed25519 等默认密钥".to_string()
            });
            let select_label = selected_path
                .as_ref()
                .map(|path| format!("更换私钥文件，当前为 {path}"))
                .unwrap_or_else(|| "选择私钥文件".to_string());
            let select_toolbar = toolbar.clone();
            let clear_toolbar = toolbar.clone();

            h_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .child(
                    Button::new("choose-key-file")
                        .outline()
                        .flex_1()
                        .min_w_0()
                        .icon(IconName::FileKey)
                        .label(key_label)
                        .accessibility_label(select_label)
                        .tooltip(key_tooltip)
                        .disabled(connected || connecting)
                        .loading(choosing_key)
                        .on_click(move |_, _, cx| {
                            let _ = select_toolbar.update(cx, |toolbar, cx| {
                                toolbar.pick_key(KeyPickerPurpose::Private, cx)
                            });
                        }),
                )
                .when(selected_path.is_some(), |this| {
                    this.child(
                        Button::new("clear-key-file")
                            .ghost()
                            .icon(IconName::X)
                            .accessibility_label("清除私钥文件，改为自动查找")
                            .tooltip("清除选择并自动查找默认密钥")
                            .disabled(connected || connecting || choosing_key)
                            .on_click(move |_, _, cx| {
                                let _ = clear_toolbar.update(cx, |toolbar, cx| {
                                    toolbar.key_path = None;
                                    toolbar.error = None;
                                    cx.notify();
                                });
                            }),
                    )
                })
                .into_any_element()
        } else {
            Input::new(&self.password)
                .id("toolbar-password")
                .disabled(form_locked)
                .w_full()
                .into_any_element()
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
                            .child("Blip"),
                    ),
            )
            .child(
                Button::new("favorites")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Bookmark)
                    .accessibility_label("收藏夹")
                    .tooltip("收藏夹")
                    .disabled(form_locked)
                    .dropdown_menu(move |menu, _window, _cx| {
                        if profiles.is_empty() {
                            return menu.min_w(px(180.)).item(
                                PopupMenuItem::new("暂无收藏，连接成功后会自动添加").disabled(true),
                            );
                        }

                        profiles.iter().enumerate().fold(
                            menu.min_w(px(260.)).max_h(px(320.)).scrollable(true),
                            |menu, (index, profile)| {
                                let profile = profile.clone();
                                let item_toolbar = toolbar.clone();
                                let delete_profile = profile.clone();
                                let delete_toolbar = toolbar.clone();
                                let icon = match profile.auth {
                                    SavedAuth::Password => IconName::Lock,
                                    SavedAuth::Key { .. } => IconName::KeyRound,
                                };
                                let label = format!(
                                    "{}@{}:{}",
                                    profile.username, profile.host, profile.port
                                );
                                let delete_label = format!("删除收藏 {label}");

                                menu.item(
                                    PopupMenuItem::element(move |_, _| {
                                        let delete_profile = delete_profile.clone();
                                        let delete_toolbar = delete_toolbar.clone();
                                        h_flex()
                                            .group("favorite-row")
                                            .flex_1()
                                            .min_w_0()
                                            .justify_between()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .child(SharedString::from(label.clone())),
                                            )
                                            .child(
                                                Button::new(format!("delete-favorite-{index}"))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::X)
                                                    .invisible()
                                                    .group_hover("favorite-row", |this| {
                                                        this.visible()
                                                    })
                                                    .focus(|this| this.visible())
                                                    .accessibility_label(delete_label.clone())
                                                    .tooltip("删除收藏")
                                                    .on_click(move |_, window, cx| {
                                                        cx.stop_propagation();
                                                        let result = delete_toolbar.update(
                                                            cx,
                                                            |toolbar, cx| {
                                                                toolbar.remove_profile(
                                                                    &delete_profile,
                                                                    cx,
                                                                )
                                                            },
                                                        );
                                                        if let Ok(Err(message)) = result {
                                                            window.push_notification(
                                                                Notification::error(message),
                                                                cx,
                                                            );
                                                        }
                                                        window
                                                            .dispatch_action(Box::new(Cancel), cx);
                                                    }),
                                            )
                                    })
                                    .icon(icon)
                                    .on_click(
                                        move |_, window, cx| {
                                            let profile = profile.clone();
                                            let _ = item_toolbar.update(cx, |toolbar, cx| {
                                                toolbar.connect_profile(&profile, window, cx)
                                            });
                                        },
                                    ),
                                )
                            },
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div().flex_1().min_w(px(140.)).child(
                            Input::new(&self.host)
                                .id("toolbar-host")
                                .disabled(form_locked)
                                .w_full(),
                        ),
                    )
                    .child(
                        div().w(px(72.)).child(
                            Input::new(&self.port)
                                .id("toolbar-port")
                                .disabled(form_locked)
                                .w_full(),
                        ),
                    )
                    .child(
                        div().w(px(150.)).child(
                            Input::new(&self.username)
                                .id("toolbar-user")
                                .disabled(form_locked)
                                .w_full(),
                        ),
                    )
                    .child(div().flex_1().min_w(px(150.)).child(password_or_key))
                    // 私钥模式才出现的口令框：加密私钥没有它根本连不上。
                    .when(use_key, |this| {
                        this.child(
                            div().w(px(120.)).flex_shrink_0().child(
                                Input::new(&self.key_passphrase)
                                    .id("toolbar-passphrase")
                                    .disabled(form_locked)
                                    .w_full(),
                            ),
                        )
                    })
                    .child(
                        Button::new("toggle-auth-mode")
                            .ghost()
                            .xsmall()
                            .icon(IconName::KeyRound)
                            .accessibility_label(if connected && !use_key {
                                "部署公钥并切换为密钥认证"
                            } else if use_key {
                                "切换为密码认证"
                            } else {
                                "切换为私钥认证"
                            })
                            .tooltip(if connected && !use_key {
                                "一键部署公钥，下次使用密钥登录"
                            } else if use_key {
                                "当前：私钥认证，点击切换为密码"
                            } else {
                                "当前：密码认证，点击切换为私钥"
                            })
                            .disabled(
                                connecting
                                    || (connected && use_key)
                                    || self.choosing_key
                                    || self.deploying_key,
                            )
                            .loading(self.choosing_key || self.deploying_key)
                            .on_click(cx.listener(|toolbar, _, _, cx| {
                                if toolbar.state == SessionState::Connected {
                                    if !toolbar.use_key {
                                        toolbar.pick_key(KeyPickerPurpose::Public, cx);
                                    }
                                } else {
                                    toolbar.use_key = !toolbar.use_key;
                                    cx.notify();
                                }
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
                            .accessibility_label(if self.remember {
                                "关闭连接信息记忆"
                            } else {
                                "记住连接信息"
                            })
                            .tooltip(match (use_key, self.remember) {
                                (true, true) => "记住这台主机（私钥路径会保存，口令不会）",
                                (true, false) => "不保存连接信息",
                                (false, true) => "密码会存入 macOS 钥匙串",
                                (false, false) => "不保存密码",
                            })
                            .disabled(form_locked)
                            .on_click(cx.listener(|toolbar, _, _, cx| {
                                toolbar.remember = !toolbar.remember;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                Button::new("settings")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Settings)
                    .accessibility_label("设置")
                    .tooltip("设置")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ToolbarEvent::OpenSettings))),
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
                            toolbar.choosing_key = false;
                            toolbar.deploying_key = false;
                            cx.notify();
                            cx.emit(ToolbarEvent::Disconnect);
                        }
                        SessionState::Connecting => {}
                        SessionState::Disconnected => toolbar.start_connect(cx),
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

fn private_key_initial_directory(selected: Option<&std::path::Path>) -> Option<PathBuf> {
    selected
        .and_then(std::path::Path::parent)
        .filter(|path| path.is_dir())
        .map(std::path::Path::to_path_buf)
        .or_else(|| {
            dirs::home_dir().and_then(|home| {
                let ssh = home.join(".ssh");
                ssh.is_dir().then_some(ssh).or(Some(home))
            })
        })
}

fn deployment_key_pair(public_key: &std::path::Path) -> Result<PathBuf, String> {
    if !public_key.is_file()
        || !public_key
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pub"))
    {
        return Err("请选择 OpenSSH .pub 公钥文件".to_string());
    }
    let private_key = public_key.with_extension("");
    if !private_key.is_file() {
        return Err(format!(
            "找不到配套私钥 {}，请把公钥与私钥放在同一目录",
            private_key.display()
        ));
    }
    Ok(private_key)
}

#[cfg(target_os = "macos")]
struct KeyPickerResult {
    path: Option<PathBuf>,
    show_hidden_files: bool,
}

/// GPUI 的通用路径选择 API 没有“显示隐藏文件”选项，因此密钥选择在 macOS 上
/// 使用原生 `NSOpenPanel`。面板内的复选框与 `showsHiddenFiles` 双向绑定，用户也可
/// 随时用系统快捷键 Command-Shift-. 切换，两种方式会保持同步。
#[cfg(target_os = "macos")]
fn prompt_for_key_file(
    prompt: &str,
    show_hidden_files: bool,
    initial_directory: Option<&std::path::Path>,
) -> flume::Receiver<Result<KeyPickerResult, String>> {
    let (done_tx, done_rx) = flume::bounded(1);
    let Some(mtm) = MainThreadMarker::new() else {
        let _ = done_tx.send(Err("只能从主线程打开密钥文件选择器".to_string()));
        return done_rx;
    };

    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(false);
    panel.setCanChooseFiles(true);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(false);
    panel.setResolvesAliases(false);
    panel.setShowsHiddenFiles(show_hidden_files);
    panel.setPrompt(Some(&NSString::from_str(prompt)));

    if let Some(directory) = initial_directory {
        let directory_url = NSURL::from_directory_path(directory);
        panel.setDirectoryURL(directory_url.as_deref());
    }

    let show_hidden = unsafe {
        NSButton::checkboxWithTitle_target_action(
            ns_string!("显示隐藏文件（⌘⇧.）"),
            None,
            None,
            mtm,
        )
    };
    show_hidden.setState(if show_hidden_files {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    show_hidden.sizeToFit();
    show_hidden.setFrameOrigin(NSPoint::new(0.0, 7.0));
    unsafe {
        show_hidden.bind_toObject_withKeyPath_options(
            NSValueBinding,
            &panel,
            ns_string!("showsHiddenFiles"),
            None,
        );
    }

    // `NSOpenPanel` may collapse a bare control used as its accessory view. A
    // fixed-size container gives AppKit an unambiguous layout area and keeps
    // the checkbox visible in both expanded and compact panel layouts.
    let accessory = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(260.0, 32.0)),
    );
    accessory.addSubview(&show_hidden);
    panel.setAccessoryView(Some(&accessory));
    panel.setAccessoryViewDisclosed(true);

    let handler = RcBlock::new({
        let panel = panel.clone();
        let show_hidden = show_hidden.clone();
        move |response: NSModalResponse| {
            let path = (response == NSModalResponseOK).then(|| {
                panel
                    .URLs()
                    .iter()
                    .find(|url| url.isFileURL())
                    .and_then(|url| url.to_file_path())
            });
            show_hidden.unbind(unsafe { NSValueBinding });
            let _ = done_tx.send(Ok(KeyPickerResult {
                path: path.flatten(),
                show_hidden_files: panel.showsHiddenFiles(),
            }));
        }
    });
    panel.beginWithCompletionHandler(&handler);

    done_rx
}
