use std::{ops::Range, path::PathBuf, sync::Arc, time::Duration};

use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, Image, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, PathPromptOptions, Render, ScrollStrategy,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, div, img, prelude::FluentBuilder as _, px, rgb,
    uniform_list,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::{DialogAction, DialogButtonProps, DialogClose, DialogFooter},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    notification::Notification,
    progress::Progress,
    spinner::Spinner,
    switch::Switch,
    v_flex,
};
use gpui_mobile::components::material::{ListTile, MaterialTheme, NavigationBarBuilder, TopAppBar};
use gpui_mobile::{StatusBarContentStyle, SystemChromeStyle, set_system_chrome};

use crate::discovery::{DiscoveredDevice, DiscoveryService};
use crate::http_share::HttpServer;
use crate::qr::qr_code_image;
use crate::remote_path;
use crate::session::store::{self, AppSettings, LanSftpPasswordMode, SavedAuth, SavedProfile};
use crate::sftp::SftpWorker;
use crate::sftp::lan_server::{DEFAULT_PASSWORD, ServerInfo, TransferActivity};
use crate::sftp::session::format_size;
use crate::sftp::types::{
    AuthMethod, EntryKind, ErrorContext, ExistingPolicy, FileEntry, HostKeyDecision, HostKeyPrompt,
    SessionConfig, SftpCommand, SftpEvent, TransferDirection, TransferId, TransferOutcome,
    UploadConflict, UploadDecision,
};
use crate::update::AvailableUpdate;

const ICON_FOLDER: &[u8] = include_bytes!("../assets/icons/folder.svg");
const ICON_FILE: &[u8] = include_bytes!("../assets/icons/file.svg");
const ICON_LINK: &[u8] = include_bytes!("../assets/icons/link.svg");
const ICON_CHEVRON_RIGHT: &[u8] = include_bytes!("../assets/icons/chevron-right.svg");
const ICON_ELLIPSIS_VERTICAL: &[u8] = include_bytes!("../assets/icons/ellipsis-vertical.svg");
const ICON_CORNER_LEFT_UP: &[u8] = include_bytes!("../assets/icons/corner-left-up.svg");
const CONNECTED_NOTIFICATION_DURATION: Duration = Duration::from_secs(3);

struct ConnectedNotification;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Screen {
    Files,
    Transfers,
    #[default]
    Connections,
    Settings,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

impl ConnectionState {
    fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "未连接",
            Self::Connecting => "正在连接…",
            Self::Connected => "已连接",
            Self::Reconnecting => "正在重连…",
        }
    }
}

#[derive(Clone, Debug)]
enum TransferState {
    Queued,
    Awaiting,
    Running,
    Completed,
    Partial(String),
    Failed(String),
    Cancelled,
    Skipped,
}

impl TransferState {
    fn label(&self) -> String {
        match self {
            Self::Queued => "等待中".to_string(),
            Self::Awaiting => "等待选择".to_string(),
            Self::Running => "传输中".to_string(),
            Self::Completed => "已完成".to_string(),
            Self::Partial(message) => format!("部分完成 · {message}"),
            Self::Failed(message) => format!("失败 · {message}"),
            Self::Cancelled => "已取消".to_string(),
            Self::Skipped => "已跳过".to_string(),
        }
    }

    fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Awaiting | Self::Running)
    }

    fn is_clearable(&self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Skipped)
    }
}

#[derive(Clone, Debug)]
struct TransferRow {
    id: TransferId,
    name: String,
    direction: TransferDirection,
    total: u64,
    transferred: u64,
    speed: f64,
    completed_files: usize,
    total_files: usize,
    state: TransferState,
}

impl TransferRow {
    fn ratio(&self) -> f32 {
        if self.total == 0 {
            return 0.;
        }
        (self.transferred as f64 / self.total as f64).clamp(0., 1.) as f32
    }
}

struct ConnectionDraft {
    use_private_key: bool,
    remember_secret: bool,
    error: Option<SharedString>,
}

struct MasterPasswordDraft {
    error: Option<SharedString>,
}

struct ConflictDraft {
    apply_to_all: bool,
}

struct SettingsDraft {
    manual_password: bool,
    error: Option<SharedString>,
}

pub struct MobileApp {
    screen: Screen,
    dark_mode: bool,
    connection: ConnectionState,
    endpoint: Option<String>,
    pending_config: Option<SessionConfig>,
    pending_remember_secret: bool,

    worker: SftpWorker,
    next_transfer_id: TransferId,
    next_request_id: u64,
    active_request_id: u64,
    remote_path: String,
    remote_home: String,
    entries: Vec<FileEntry>,
    remote_loading: bool,
    remote_error: Option<SharedString>,
    remote_scroll: UniformListScrollHandle,
    transfers: Vec<TransferRow>,

    profiles: Vec<SavedProfile>,
    discovered: Vec<DiscoveredDevice>,
    settings: AppSettings,
    unlocked: bool,
    discovery: Option<DiscoveryService>,
    lan_server: Result<ServerInfo, SharedString>,
    http_server: Result<HttpServer, SharedString>,
    http_qr_code: Option<(Arc<Image>, f32)>,
    lan_activity: TransferActivity,

    host: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    private_key: Entity<InputState>,
    key_passphrase: Entity<InputState>,

    status_message: Option<SharedString>,
    available_update: Option<AvailableUpdate>,
    _subscriptions: Vec<Subscription>,
    _event_pump: Task<()>,
    _discovery_pump: Option<Task<()>>,
    _background_tasks: Vec<Task<()>>,
}

impl MobileApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let worker = SftpWorker::spawn();
        let events = worker.events().clone();
        let event_pump = cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = events.recv_async().await {
                if this
                    .update_in(cx, |app, window, cx| app.handle_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let host = cx.new(|cx| InputState::new(window, cx).placeholder("主机或 IP"));
        let port = cx.new(|cx| InputState::new(window, cx).default_value("22"));
        let username = cx.new(|cx| InputState::new(window, cx).placeholder("用户名"));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(DEFAULT_PASSWORD)
                .placeholder("密码")
                .masked(true)
        });
        let private_key =
            cx.new(|cx| InputState::new(window, cx).placeholder("私钥路径；留空自动查找"));
        let key_passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("密钥口令（可选）")
                .masked(true)
        });

        let mut subscriptions = Vec::new();
        for input in [
            &host,
            &port,
            &username,
            &password,
            &private_key,
            &key_passphrase,
        ] {
            subscriptions.push(
                cx.subscribe_in(input, window, |_, _, event, _, _| match event {
                    InputEvent::Focus => gpui_mobile::show_keyboard(),
                    InputEvent::Blur => gpui_mobile::hide_keyboard(),
                    _ => {}
                }),
            );
        }

        let (lan_activity, _) = crate::sftp::lan_server::transfer_activity();
        let auto_unlocked = store::try_auto_unlock_config().unwrap_or(false);
        let mut app = Self {
            screen: Screen::default(),
            dark_mode: true,
            connection: ConnectionState::Disconnected,
            endpoint: None,
            pending_config: None,
            pending_remember_secret: true,
            worker,
            next_transfer_id: 1,
            next_request_id: 1,
            active_request_id: 0,
            remote_path: "/".to_string(),
            remote_home: "/".to_string(),
            entries: Vec::new(),
            remote_loading: false,
            remote_error: None,
            remote_scroll: UniformListScrollHandle::new(),
            transfers: Vec::new(),
            profiles: Vec::new(),
            discovered: Vec::new(),
            settings: AppSettings::default(),
            unlocked: auto_unlocked,
            discovery: None,
            lan_server: Err("配置尚未解锁".into()),
            http_server: Err("配置尚未解锁".into()),
            http_qr_code: None,
            lan_activity,
            host,
            port,
            username,
            password,
            private_key,
            key_passphrase,
            status_message: None,
            available_update: None,
            _subscriptions: subscriptions,
            _event_pump: event_pump,
            _discovery_pump: None,
            _background_tasks: Vec::new(),
        };

        if auto_unlocked {
            app.load_unlocked_services(cx);
        } else {
            let app_entity = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                Self::open_master_password_dialog(app_entity, window, cx)
            });
        }
        app.check_for_updates(cx);
        app
    }

    fn check_for_updates(&self, cx: &mut Context<Self>) {
        let http_client = cx.http_client();
        let current_version = env!("CARGO_PKG_VERSION").to_string();
        cx.spawn(async move |this, cx| {
            match crate::update::check_latest_release(http_client, &current_version).await {
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
                Err(error) => log::debug!("移动端检查更新失败：{error:#}"),
            }
        })
        .detach();
    }

    fn open_update_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(available) = self.available_update.clone() else {
            return;
        };
        window.open_dialog(cx, move |dialog, _, _| {
            let release_url = available.release_url.clone();
            dialog
                .title("发现新版本")
                .overlay_closable(false)
                .child(format!(
                    "Blip {} 已发布，当前版本为 {}。",
                    available.version, available.current_version
                ))
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
                                .label("打开下载页")
                                .on_click(move |_, window, cx| {
                                    let _ = gpui_mobile::packages::url_launcher::launch_url(
                                        &release_url,
                                    );
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn load_unlocked_services(&mut self, cx: &mut Context<Self>) {
        self.unlocked = true;
        self.profiles = store::load_profiles();
        store::sort_profiles_for_display(&mut self.profiles);
        self.settings = store::load_settings();

        let password = match self.settings.lan_sftp_password_mode {
            LanSftpPasswordMode::Default => None,
            LanSftpPasswordMode::Manual => store::load_lan_sftp_password().ok().flatten(),
        };
        self.lan_server = crate::sftp::lan_server::start(
            self.settings.lan_sftp_root.clone(),
            &self.settings.lan_sftp_username,
            password.clone(),
            self.lan_activity.clone(),
        )
        .map_err(SharedString::from);
        self.http_server = crate::http_share::start(
            self.settings.lan_sftp_root.clone(),
            &self.settings.lan_sftp_username,
            password,
            self.settings.lan_http_port,
            self.lan_activity.clone(),
        )
        .map_err(SharedString::from);
        self.refresh_http_qr_code();

        match crate::discovery::start(
            self.lan_server.as_ref().ok().map(|server| server.port),
            self.settings.discovery_username.as_deref(),
            self.settings.discovery_port,
        ) {
            Ok(discovery) => {
                let events = discovery.events().clone();
                self._discovery_pump = Some(cx.spawn(async move |this, cx| {
                    while let Ok(devices) = events.recv_async().await {
                        if this
                            .update(cx, |app, cx| {
                                app.discovered = devices;
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
                self.discovery = Some(discovery);
            }
            Err(message) => self.status_message = Some(message.into()),
        }
        cx.notify();
    }

    fn apply_http_settings(&mut self, password: Option<String>) -> Result<(), String> {
        if let Ok(server) = &self.http_server
            && server.port() == self.settings.lan_http_port
        {
            server.set_username(&self.settings.lan_sftp_username)?;
            server.set_password(password)?;
            self.refresh_http_qr_code();
            return Ok(());
        }

        match crate::http_share::start(
            self.settings.lan_sftp_root.clone(),
            &self.settings.lan_sftp_username,
            password,
            self.settings.lan_http_port,
            self.lan_activity.clone(),
        ) {
            Ok(server) => {
                self.http_server = Ok(server);
                self.refresh_http_qr_code();
                Ok(())
            }
            Err(message) => {
                if self.http_server.is_err() {
                    self.http_server = Err(message.clone().into());
                    self.http_qr_code = None;
                }
                Err(message)
            }
        }
    }

    fn refresh_http_qr_code(&mut self) {
        self.http_qr_code =
            self.http_server
                .as_ref()
                .ok()
                .and_then(|server| match qr_code_image(&server.url()) {
                    Ok(image) => Some(image),
                    Err(error) => {
                        log::warn!("生成移动端 HTTP 共享二维码失败：{error}");
                        None
                    }
                });
    }

    fn open_master_password_dialog(app: WeakEntity<Self>, window: &mut Window, cx: &mut App) {
        let creating = !store::is_config_initialized();
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(if creating {
                    "设置主密码"
                } else {
                    "输入主密码"
                })
                .masked(true)
        });
        let confirmation = creating.then(|| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("再次输入主密码")
                    .masked(true)
            })
        });
        let draft = cx.new(|_| MasterPasswordDraft { error: None });
        for input in std::iter::once(&password).chain(confirmation.iter()) {
            cx.subscribe(input, |_, event: &InputEvent, _| match event {
                InputEvent::Focus => gpui_mobile::show_keyboard(),
                InputEvent::Blur => gpui_mobile::hide_keyboard(),
                _ => {}
            })
            .detach();
        }

        window.open_dialog(cx, move |dialog, _, cx| {
            let error = draft.read(cx).error.clone();
            let app = app.clone();
            let password_for_save = password.clone();
            let confirmation_for_save = confirmation.clone();
            let draft_for_save = draft.clone();
            dialog
                .title(if creating {
                    "保护 Blip 配置"
                } else {
                    "解锁 Blip"
                })
                .close_button(false)
                .overlay_closable(false)
                .keyboard(false)
                .child(
                    v_flex()
                        .gap_3()
                        .child("收藏、设置和凭据会使用此主密码加密。")
                        .child(Input::new(&password).mask_toggle())
                        .when_some(confirmation.as_ref(), |this, confirmation| {
                            this.child(Input::new(confirmation).mask_toggle())
                        })
                        .when_some(error, |this, error| {
                            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                        }),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(if creating {
                            "保存并解锁"
                        } else {
                            "解锁"
                        })
                        .on_ok(move |_, window, cx| {
                            let secret = password_for_save.read(cx).value().to_string();
                            let result = if creating {
                                let repeated = confirmation_for_save
                                    .as_ref()
                                    .map(|input| input.read(cx).value().to_string())
                                    .unwrap_or_default();
                                if secret != repeated {
                                    Err("两次输入的主密码不一致".to_string())
                                } else {
                                    store::initialize_config(&secret)
                                }
                            } else {
                                store::unlock_config(&secret)
                            };
                            match result {
                                Ok(()) => {
                                    gpui_mobile::hide_keyboard();
                                    let _ =
                                        app.update(cx, |app, cx| app.load_unlocked_services(cx));
                                    window.push_notification("配置已解锁", cx);
                                    true
                                }
                                Err(message) => {
                                    draft_for_save.update(cx, |draft, cx| {
                                        draft.error = Some(message.into());
                                        cx.notify();
                                    });
                                    false
                                }
                            }
                        }),
                )
                .footer(Self::dialog_footer(
                    "master-password-submit",
                    if creating {
                        "保存并解锁"
                    } else {
                        "解锁"
                    },
                    false,
                ))
        });
    }

    fn input_value(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_string()
    }

    fn set_input(
        input: &Entity<InputState>,
        value: impl Into<String>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let value = value.into();
        input.update(cx, |input, cx| input.set_value(value, window, cx));
    }

    fn dialog_footer(
        submit_id: &'static str,
        submit_label: impl Into<SharedString>,
        show_cancel: bool,
    ) -> DialogFooter {
        DialogFooter::new()
            .when(show_cancel, |footer| {
                footer
                    .child(DialogClose::new().trigger(|button| button.large().h_12().label("取消")))
            })
            .child(
                DialogAction::new().child(
                    Button::new(submit_id)
                        .large()
                        .h_12()
                        .primary()
                        .label(submit_label),
                ),
            )
    }

    fn open_connection_dialog(
        app: WeakEntity<Self>,
        profile: Option<SavedProfile>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(app_snapshot) = app.upgrade() else {
            return;
        };
        let (host, port, username, password, private_key, key_passphrase) = {
            let app = app_snapshot.read(cx);
            (
                app.host.clone(),
                app.port.clone(),
                app.username.clone(),
                app.password.clone(),
                app.private_key.clone(),
                app.key_passphrase.clone(),
            )
        };

        let mut use_private_key = false;
        let mut remember_secret = false;
        if let Some(profile) = &profile {
            Self::set_input(&host, profile.host.clone(), window, cx);
            Self::set_input(&port, profile.port.to_string(), window, cx);
            Self::set_input(&username, profile.username.clone(), window, cx);
            match &profile.auth {
                SavedAuth::Password => {
                    let config = SessionConfig {
                        host: profile.host.clone(),
                        port: profile.port,
                        username: profile.username.clone(),
                        auth: AuthMethod::Password(String::new()),
                    };
                    let saved_password = store::load_password(&config).ok().flatten();
                    remember_secret = saved_password.is_some();
                    Self::set_input(
                        &password,
                        saved_password.unwrap_or_else(|| DEFAULT_PASSWORD.to_string()),
                        window,
                        cx,
                    );
                }
                SavedAuth::Key { path } => {
                    use_private_key = true;
                    Self::set_input(&private_key, path.clone(), window, cx);
                    let config = SessionConfig {
                        host: profile.host.clone(),
                        port: profile.port,
                        username: profile.username.clone(),
                        auth: AuthMethod::PrivateKey {
                            path: PathBuf::from(path),
                            passphrase: None,
                        },
                    };
                    let saved_passphrase = store::load_key_passphrase(&config).ok().flatten();
                    remember_secret = saved_passphrase.is_some();
                    Self::set_input(
                        &key_passphrase,
                        saved_passphrase.unwrap_or_default(),
                        window,
                        cx,
                    );
                }
            }
        } else {
            Self::set_input(&password, DEFAULT_PASSWORD, window, cx);
            Self::set_input(&private_key, "", window, cx);
            Self::set_input(&key_passphrase, "", window, cx);
        }
        let draft = cx.new(|_| ConnectionDraft {
            use_private_key,
            remember_secret,
            error: None,
        });

        window.open_dialog(cx, move |dialog, _, cx| {
            let snapshot = draft.read(cx);
            let use_private_key = snapshot.use_private_key;
            let remember_secret = snapshot.remember_secret;
            let error = snapshot.error.clone();
            let app = app.clone();
            let draft_for_save = draft.clone();
            let host_for_save = host.clone();
            let port_for_save = port.clone();
            let username_for_save = username.clone();
            let password_for_save = password.clone();
            let private_key_for_save = private_key.clone();
            let private_key_for_picker = private_key.clone();
            let key_passphrase_for_save = key_passphrase.clone();

            dialog
                .title("连接 SFTP")
                .child(
                    v_flex()
                        .gap_3()
                        .child(Input::new(&host).prefix("主机"))
                        .child(Input::new(&port).prefix("端口"))
                        .child(Input::new(&username).prefix("用户"))
                        .child(
                            Switch::new("connection-auth-mode")
                                .label("使用私钥")
                                .checked(use_private_key)
                                .on_change({
                                    let draft = draft.clone();
                                    move |checked, _, cx| {
                                        draft.update(cx, |draft, cx| {
                                            draft.use_private_key = *checked;
                                            draft.error = None;
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                        .when(use_private_key, |this| {
                            this.child(Input::new(&private_key).prefix("私钥"))
                                .child(
                                    Button::new("choose-private-key")
                                        .label("从“文件”中选择私钥…")
                                        .on_click({
                                            let input = private_key_for_picker.clone();
                                            move |_, window, cx| {
                                                let receiver =
                                                    cx.prompt_for_paths(PathPromptOptions {
                                                        files: true,
                                                        directories: false,
                                                        multiple: false,
                                                        prompt: Some("选择 SSH 私钥".into()),
                                                    });
                                                let window_handle = window.window_handle();
                                                let input = input.clone();
                                                cx.spawn(async move |cx| {
                                                    if let Ok(Ok(Some(paths))) = receiver.await
                                                        && let Some(path) = paths.into_iter().next()
                                                    {
                                                        let _ = cx.update_window(
                                                            window_handle,
                                                            |_, window, cx| {
                                                                Self::set_input(
                                                                    &input,
                                                                    path.to_string_lossy(),
                                                                    window,
                                                                    cx,
                                                                )
                                                            },
                                                        );
                                                    }
                                                })
                                                .detach();
                                            }
                                        }),
                                )
                                .child(Input::new(&key_passphrase).prefix("口令").mask_toggle())
                        })
                        .when(!use_private_key, |this| {
                            this.child(Input::new(&password).prefix("密码").mask_toggle())
                        })
                        .child(
                            Checkbox::new("remember-connection-secret")
                                .label(if use_private_key {
                                    "保存私钥口令"
                                } else {
                                    "保存密码"
                                })
                                .checked(remember_secret)
                                .on_change({
                                    let draft = draft.clone();
                                    move |checked, _, cx| {
                                        draft.update(cx, |draft, cx| {
                                            draft.remember_secret = *checked;
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                        .when_some(error, |this, error| {
                            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                        }),
                )
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text("连接")
                        .on_ok(move |_, _, cx| {
                            let host = Self::input_value(&host_for_save, cx);
                            let port = Self::input_value(&port_for_save, cx).parse::<u16>().ok();
                            let username = Self::input_value(&username_for_save, cx);
                            let (use_private_key, remember_secret) = {
                                let draft_snapshot = draft_for_save.read(cx);
                                (
                                    draft_snapshot.use_private_key,
                                    draft_snapshot.remember_secret,
                                )
                            };

                            let error = if host.is_empty() {
                                Some("请填写主机地址")
                            } else if port.is_none() || port == Some(0) {
                                Some("端口必须是 1–65535 之间的整数")
                            } else if username.is_empty() {
                                Some("请填写用户名")
                            } else if !use_private_key
                                && Self::input_value(&password_for_save, cx).is_empty()
                            {
                                Some("请填写密码")
                            } else {
                                None
                            };
                            if let Some(error) = error {
                                draft_for_save.update(cx, |draft, cx| {
                                    draft.error = Some(error.into());
                                    cx.notify();
                                });
                                return false;
                            }

                            let auth = if use_private_key {
                                AuthMethod::PrivateKey {
                                    path: PathBuf::from(Self::input_value(
                                        &private_key_for_save,
                                        cx,
                                    )),
                                    passphrase: match Self::input_value(
                                        &key_passphrase_for_save,
                                        cx,
                                    ) {
                                        passphrase if passphrase.is_empty() => None,
                                        passphrase => Some(passphrase),
                                    },
                                }
                            } else {
                                AuthMethod::Password(Self::input_value(&password_for_save, cx))
                            };
                            let config = SessionConfig {
                                host,
                                port: port.expect("validated port"),
                                username,
                                auth,
                            };
                            let _ =
                                app.update(cx, |app, cx| app.connect(config, remember_secret, cx));
                            gpui_mobile::hide_keyboard();
                            true
                        }),
                )
                .footer(Self::dialog_footer("connection-submit", "连接", true))
        });
    }

    fn open_profile_actions(
        app: WeakEntity<Self>,
        profile: SavedProfile,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title = profile.title();
        window.open_dialog(cx, move |dialog, _, _| {
            let connect_app = app.clone();
            let pin_app = app.clone();
            let rename_app = app.clone();
            let delete_app = app.clone();
            let connect_profile = profile.clone();
            let pin_profile = profile.clone();
            let rename_profile = profile.clone();
            let delete_profile = profile.clone();

            dialog.title(title.clone()).child(
                v_flex()
                    .gap_2()
                    .child(
                        Button::new("connect-saved-profile")
                            .primary()
                            .label("连接")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                Self::open_connection_dialog(
                                    connect_app.clone(),
                                    Some(connect_profile.clone()),
                                    window,
                                    cx,
                                );
                            }),
                    )
                    .child(
                        Button::new("pin-saved-profile")
                            .label(if profile.pinned {
                                "取消置顶"
                            } else {
                                "置顶"
                            })
                            .on_click(move |_, window, cx| {
                                let result = pin_app.update(cx, |app, cx| {
                                    let mut profiles = app.profiles.clone();
                                    let Some(saved) = profiles.iter_mut().find(|saved| {
                                        saved.host == pin_profile.host
                                            && saved.port == pin_profile.port
                                            && saved.username == pin_profile.username
                                    }) else {
                                        return Err("连接配置已不存在".to_string());
                                    };
                                    saved.pinned = !saved.pinned;
                                    store::save_profiles(&profiles)?;
                                    store::sort_profiles_for_display(&mut profiles);
                                    app.profiles = profiles;
                                    cx.notify();
                                    Ok(())
                                });
                                if let Err(message) =
                                    result.unwrap_or_else(|_| Err("无法更新连接配置".to_string()))
                                {
                                    window.push_notification(message, cx);
                                }
                                window.close_dialog(cx);
                            }),
                    )
                    .child(Button::new("rename-saved-profile").label("命名…").on_click(
                        move |_, window, cx| {
                            window.close_dialog(cx);
                            Self::open_profile_name_dialog(
                                rename_app.clone(),
                                rename_profile.clone(),
                                window,
                                cx,
                            );
                        },
                    ))
                    .child(
                        Button::new("delete-saved-profile")
                            .danger()
                            .label("删除…")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                Self::open_profile_delete_dialog(
                                    delete_app.clone(),
                                    delete_profile.clone(),
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
        });
    }

    fn open_profile_name_dialog(
        app: WeakEntity<Self>,
        profile: SavedProfile,
        window: &mut Window,
        cx: &mut App,
    ) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(profile.name.clone().unwrap_or_default())
                .placeholder("例如：家里的 NAS")
        });
        window.open_dialog(cx, move |dialog, _, _| {
            let app = app.clone();
            let name = name.clone();
            let profile = profile.clone();
            dialog
                .title("连接名称")
                .child(Input::new(&name))
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text("保存")
                        .on_ok(move |_, window, cx| {
                            let value = name.read(cx).value().trim().to_string();
                            let result = app.update(cx, |app, cx| {
                                let mut profiles = app.profiles.clone();
                                let Some(saved) = profiles.iter_mut().find(|saved| {
                                    saved.host == profile.host
                                        && saved.port == profile.port
                                        && saved.username == profile.username
                                }) else {
                                    return Err("连接配置已不存在".to_string());
                                };
                                saved.name = (!value.is_empty()).then_some(value.clone());
                                store::save_profiles(&profiles)?;
                                app.profiles = profiles;
                                cx.notify();
                                Ok(())
                            });
                            if let Err(message) =
                                result.unwrap_or_else(|_| Err("无法更新连接配置".to_string()))
                            {
                                window.push_notification(message, cx);
                                return false;
                            }
                            gpui_mobile::hide_keyboard();
                            true
                        }),
                )
                .footer(Self::dialog_footer("profile-name-submit", "保存", true))
        });
    }

    fn open_profile_delete_dialog(
        app: WeakEntity<Self>,
        profile: SavedProfile,
        window: &mut Window,
        cx: &mut App,
    ) {
        let deleting = cx.new(|_| false);
        window.open_dialog(cx, move |dialog, _, cx| {
            let is_deleting = *deleting.read(cx);
            let app = app.clone();
            let profile = profile.clone();
            let cancel_deleting = deleting.clone();
            let submit_deleting = deleting.clone();
            dialog
                .title(format!("删除“{}”？", profile.title()))
                .overlay_closable(!is_deleting)
                .child("保存的连接信息和对应凭据会一并移除。")
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("cancel-profile-delete")
                                .label("取消")
                                .disabled(is_deleting)
                                .on_click(move |_, window, cx| {
                                    if !*cancel_deleting.read(cx) {
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .child(
                            Button::new("confirm-profile-delete")
                                .danger()
                                .label(if is_deleting {
                                    "删除中…"
                                } else {
                                    "删除"
                                })
                                .loading(is_deleting)
                                .disabled(is_deleting)
                                .on_click(move |_, window, cx| {
                                    if *submit_deleting.read(cx) {
                                        return;
                                    }
                                    let prepared = app.update(cx, |app, _| {
                                        let mut profiles = app.profiles.clone();
                                        let removed = store::remove_profile(
                                            &mut profiles,
                                            &profile.username,
                                            &profile.host,
                                            profile.port,
                                        )
                                        .ok_or_else(|| "连接配置已不存在".to_string())?;
                                        Ok::<_, String>((profiles, removed))
                                    });
                                    let (profiles, removed) = match prepared
                                        .unwrap_or_else(|_| Err("无法删除连接配置".to_string()))
                                    {
                                        Ok(prepared) => prepared,
                                        Err(message) => {
                                            window.push_notification(message, cx);
                                            return;
                                        }
                                    };

                                    submit_deleting.update(cx, |deleting, cx| {
                                        *deleting = true;
                                        cx.notify();
                                    });
                                    let window_handle = window.window_handle();
                                    let app = app.clone();
                                    let deleting = submit_deleting.clone();
                                    cx.spawn(async move |cx| {
                                        let persisted_profiles = profiles.clone();
                                        let result = cx
                                            .background_executor()
                                            .spawn(async move {
                                                store::save_profile_removal(
                                                    &persisted_profiles,
                                                    &removed,
                                                )
                                            })
                                            .await;
                                        let _ = cx.update_window(window_handle, |_, window, cx| {
                                            deleting.update(cx, |deleting, cx| {
                                                *deleting = false;
                                                cx.notify();
                                            });
                                            match result {
                                                Ok(()) => {
                                                    let _ = app.update(cx, |app, cx| {
                                                        app.profiles = profiles;
                                                        cx.notify();
                                                    });
                                                    window.close_dialog(cx);
                                                }
                                                Err(message) => {
                                                    window.push_notification(message, cx)
                                                }
                                            }
                                        });
                                    })
                                    .detach();
                                }),
                        ),
                )
        });
    }

    fn connect(&mut self, config: SessionConfig, remember_secret: bool, cx: &mut Context<Self>) {
        self.connection = ConnectionState::Connecting;
        self.endpoint = Some(config.endpoint());
        self.pending_config = Some(config.clone());
        self.pending_remember_secret = remember_secret;
        self.remote_error = None;
        self.screen = Screen::Files;
        self.worker.send(SftpCommand::Connect(config));
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.worker.send(SftpCommand::Disconnect);
        self.connection = ConnectionState::Disconnected;
        self.endpoint = None;
        self.entries.clear();
        self.remote_path = "/".to_string();
        cx.notify();
    }

    fn request_directory(
        &mut self,
        path: String,
        fallback: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.connection != ConnectionState::Connected {
            return;
        }
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.active_request_id = request_id;
        self.remote_loading = true;
        self.remote_error = None;
        self.worker.send(SftpCommand::ListDir {
            request_id,
            path,
            fallback,
        });
        cx.notify();
    }

    fn refresh_directory(&mut self, cx: &mut Context<Self>) {
        self.request_directory(self.remote_path.clone(), None, cx);
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = remote_path::parent(&self.remote_path) {
            self.request_directory(parent, None, cx);
        }
    }

    fn allocate_transfer(
        &mut self,
        name: String,
        direction: TransferDirection,
        total: u64,
        cx: &mut Context<Self>,
    ) -> TransferId {
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        self.transfers.insert(
            0,
            TransferRow {
                id,
                name,
                direction,
                total,
                transferred: 0,
                speed: 0.,
                completed_files: 0,
                total_files: 0,
                state: TransferState::Queued,
            },
        );
        cx.notify();
        id
    }

    fn download(&mut self, entry: FileEntry, cx: &mut Context<Self>) {
        let documents = match gpui_mobile::packages::path_provider::documents_directory() {
            Ok(path) => path,
            Err(message) => {
                self.status_message = Some(format!("无法访问下载目录：{message}").into());
                cx.notify();
                return;
            }
        };
        let target = documents.join(&entry.name);
        let id = self.allocate_transfer(
            entry.name.clone(),
            TransferDirection::Download,
            entry.size,
            cx,
        );
        if entry.is_dir() {
            self.worker.send(SftpCommand::DownloadDir {
                id,
                remote: entry.path,
                local: target,
                on_existing: ExistingPolicy::Overwrite,
            });
        } else {
            self.worker.send(SftpCommand::Download {
                id,
                remote: entry.path,
                local: target,
                total: entry.size,
                resume: true,
            });
        }
        self.screen = Screen::Transfers;
    }

    fn pick_uploads(&mut self, cx: &mut Context<Self>) {
        let remote = self.remote_path.clone();
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("选择要上传的文件".into()),
        });
        let task = cx.spawn(async move |this, cx| {
            let selected = receiver.await;
            let _ = this.update(cx, |app, cx| match selected {
                Ok(Ok(Some(files))) => {
                    for local in files {
                        let name = local
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| "文件".to_string());
                        let total = local.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                        let id = app.allocate_transfer(
                            name.clone(),
                            TransferDirection::Upload,
                            total,
                            cx,
                        );
                        app.worker.send(SftpCommand::Upload {
                            id,
                            local,
                            remote: remote_path::join(&remote, &name),
                            total,
                            resume: true,
                        });
                    }
                    if !app.transfers.is_empty() {
                        app.screen = Screen::Transfers;
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(message)) => {
                    app.status_message = Some(format!("无法选择文件：{message}").into())
                }
                Err(_) => app.status_message = Some("文件选择器意外关闭，请重试".into()),
            });
        });
        self._background_tasks.push(task);
    }

    fn pick_and_install_public_key(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择 SSH 公钥".into()),
        });
        let task = cx.spawn(async move |this, cx| {
            let selected = receiver.await;
            let _ = this.update(cx, |app, cx| match selected {
                Ok(Ok(Some(paths))) => {
                    let Some(public_key) = paths.into_iter().next() else {
                        return;
                    };
                    let private_key = if public_key.extension().is_some_and(|ext| ext == "pub") {
                        public_key.with_extension("")
                    } else {
                        PathBuf::from(app.private_key.read(cx).value().trim())
                    };
                    if private_key.as_os_str().is_empty() || !private_key.is_file() {
                        app.status_message = Some(
                            "无法找到对应私钥：请选择 .pub 公钥，或先在连接信息中选择私钥".into(),
                        );
                        cx.notify();
                        return;
                    }
                    app.worker.send(SftpCommand::InstallPublicKey {
                        public_key,
                        private_key,
                    });
                }
                Ok(Ok(None)) => {}
                Ok(Err(message)) => {
                    app.status_message = Some(format!("无法选择公钥：{message}").into())
                }
                Err(_) => app.status_message = Some("公钥选择器意外关闭，请重试".into()),
            });
        });
        self._background_tasks.push(task);
    }

    fn pick_upload_directory(&mut self, cx: &mut Context<Self>) {
        let remote = self.remote_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let selected = cx
                .background_executor()
                .spawn(async {
                    #[cfg(target_os = "ios")]
                    {
                        gpui_mobile::packages::file_selector::get_directory_path(None)
                    }
                    #[cfg(target_os = "android")]
                    {
                        crate::android_picker::pick_directory()
                    }
                })
                .await;
            let _ = this.update(cx, |app, cx| match selected {
                Ok(Some(path)) => {
                    let local = PathBuf::from(path);
                    let name = local
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| "文件夹".to_string());
                    let id = app.allocate_transfer(name.clone(), TransferDirection::Upload, 0, cx);
                    app.worker.send(SftpCommand::UploadDir {
                        id,
                        local,
                        remote: remote_path::join(&remote, &name),
                        on_existing: ExistingPolicy::Overwrite,
                    });
                    app.screen = Screen::Transfers;
                }
                Ok(None) => {}
                Err(message) => {
                    app.status_message = Some(format!("无法选择文件夹：{message}").into())
                }
            });
        });
        self._background_tasks.push(task);
    }

    fn handle_event(&mut self, event: SftpEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            SftpEvent::Connected { home, .. } => {
                self.connection = ConnectionState::Connected;
                self.remote_home = home.clone();
                self.remote_path = home.clone();
                self.persist_successful_connection();
                self.request_directory(home, None, cx);
                let notification_id = self.active_request_id.to_string();
                window.push_notification(
                    Notification::success("SFTP 已连接")
                        .id1::<ConnectedNotification>(notification_id.clone())
                        .autohide(false),
                    cx,
                );
                cx.spawn_in(window, async move |_, cx| {
                    cx.background_executor()
                        .timer(CONNECTED_NOTIFICATION_DURATION)
                        .await;
                    let _ = cx.update(|window, cx| {
                        window.remove_notification1::<ConnectedNotification>(notification_id, cx)
                    });
                })
                .detach();
            }
            SftpEvent::Reconnecting => {
                self.connection = ConnectionState::Reconnecting;
                cx.notify();
            }
            SftpEvent::Disconnected => {
                self.connection = ConnectionState::Disconnected;
                self.entries.clear();
                cx.notify();
            }
            SftpEvent::TransferPoolChanged { .. } => {}
            SftpEvent::DirListed {
                request_id,
                path,
                entries,
            } if request_id == self.active_request_id => {
                self.remote_path = path;
                self.entries = entries;
                self.remote_loading = false;
                self.remote_error = None;
                if !self.entries.is_empty() {
                    self.remote_scroll
                        .scroll_to_item_strict(0, ScrollStrategy::Top);
                }
                cx.notify();
            }
            SftpEvent::DirListed { .. } => {}
            SftpEvent::TransferStarted {
                id,
                resumed_from,
                total,
            } => {
                if let Some(row) = self.transfer_mut(id) {
                    row.state = TransferState::Running;
                    row.transferred = resumed_from;
                    if total > 0 {
                        row.total = total;
                    }
                }
                cx.notify();
            }
            SftpEvent::TransferProgress {
                id,
                transferred,
                total,
                speed,
            } => {
                if let Some(row) = self.transfer_mut(id) {
                    row.transferred = transferred;
                    if total > 0 {
                        row.total = total;
                    }
                    if speed.is_finite() && speed >= 0. {
                        row.speed = speed;
                    }
                }
                cx.notify();
            }
            SftpEvent::TransferFileProgress {
                id,
                completed,
                total,
            } => {
                if let Some(row) = self.transfer_mut(id) {
                    row.completed_files = completed;
                    row.total_files = total;
                }
                cx.notify();
            }
            SftpEvent::TransferActiveFiles { .. } => {}
            SftpEvent::TransferCompleted { id, outcome } => {
                if let Some(row) = self.transfer_mut(id) {
                    row.speed = 0.;
                    row.state = match outcome {
                        TransferOutcome::Completed => TransferState::Completed,
                        TransferOutcome::Partial(message) => TransferState::Partial(message),
                        TransferOutcome::Failed(message) => TransferState::Failed(message),
                        TransferOutcome::Cancelled => TransferState::Cancelled,
                        TransferOutcome::Skipped => TransferState::Skipped,
                    };
                }
                if self.connection == ConnectionState::Connected {
                    self.refresh_directory(cx);
                } else {
                    cx.notify();
                }
            }
            SftpEvent::UploadConflict(conflict) => {
                if let Some(row) = self.transfer_mut(conflict.id) {
                    row.state = TransferState::Awaiting;
                }
                Self::open_upload_conflict(conflict, self.worker.sender(), window, cx);
            }
            SftpEvent::HostKeyPrompt(prompt) => {
                Self::open_host_key_prompt(prompt, self.worker.sender(), window, cx)
            }
            SftpEvent::TextLoaded { path, content } => {
                Self::open_text_editor(path, content, self.worker.sender(), window, cx)
            }
            SftpEvent::TextSaved { path } => {
                window.push_notification(format!("已保存 {}", path), cx)
            }
            SftpEvent::PublicKeyInstalled {
                already_present, ..
            } => window.push_notification(
                if already_present {
                    "公钥已存在"
                } else {
                    "公钥部署完成"
                },
                cx,
            ),
            SftpEvent::Notice(message) => window.push_notification(message, cx),
            SftpEvent::Error { context, message } => {
                if context == ErrorContext::Connect {
                    self.connection = ConnectionState::Disconnected;
                    self.pending_config = None;
                }
                if context == ErrorContext::List {
                    self.remote_loading = false;
                    self.remote_error = Some(message.clone().into());
                }
                window.push_notification(message, cx);
                cx.notify();
            }
        }
    }

    fn persist_successful_connection(&mut self) {
        let Some(config) = self.pending_config.take() else {
            return;
        };
        let auth = match &config.auth {
            AuthMethod::Password(_) => SavedAuth::Password,
            AuthMethod::PrivateKey { path, .. } => SavedAuth::Key {
                path: path.to_string_lossy().to_string(),
            },
        };
        let existing = self.profiles.iter().find(|profile| {
            profile.host == config.host
                && profile.port == config.port
                && profile.username == config.username
        });
        let profile = SavedProfile {
            name: existing.and_then(|profile| profile.name.clone()),
            host: config.host.clone(),
            port: config.port,
            username: config.username.clone(),
            auth,
            pinned: existing.is_some_and(|profile| profile.pinned),
            last_connected: Some(store::unix_timestamp()),
        };
        let update = store::upsert_profile(&mut self.profiles, profile);
        if let Err(message) = store::save_connection(
            &self.profiles,
            &config,
            self.pending_remember_secret,
            update.previous.as_ref(),
            update.evicted.as_ref(),
        ) {
            self.status_message = Some(message.into());
        }
        store::sort_profiles_for_display(&mut self.profiles);
    }

    fn transfer_mut(&mut self, id: TransferId) -> Option<&mut TransferRow> {
        self.transfers.iter_mut().find(|row| row.id == id)
    }

    fn open_host_key_prompt(
        prompt: HostKeyPrompt,
        sender: flume::Sender<SftpCommand>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let changed = prompt.changed_line.is_some();
        window.open_dialog(cx, move |dialog, _, cx| {
            let sender_reject = sender.clone();
            let sender_once = sender.clone();
            let sender_save = sender.clone();
            dialog
                .title(if changed {
                    "服务器主机密钥已变化"
                } else {
                    "确认服务器身份"
                })
                .close_button(false)
                .overlay_closable(false)
                .keyboard(false)
                .child(
                    v_flex()
                        .gap_2()
                        .child(prompt.endpoint.clone())
                        .child(format!("算法：{}", prompt.algorithm))
                        .child(format!("SHA-256：{}", prompt.fingerprint))
                        .when(changed, |this| {
                            this.child(
                                div()
                                    .text_color(cx.theme().danger)
                                    .child("已保存的密钥与服务器当前密钥不同。除非你确认服务器刚刚更换过密钥，否则应拒绝连接。"),
                            )
                        }),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("reject-host-key")
                                .danger()
                                .label("拒绝")
                                .on_click(move |_, window, cx| {
                                    let _ = sender_reject
                                        .send(SftpCommand::ResolveHostKey(HostKeyDecision::Reject));
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("trust-host-key-once")
                                .label("仅本次信任")
                                .on_click(move |_, window, cx| {
                                    let _ = sender_once.send(SftpCommand::ResolveHostKey(
                                        HostKeyDecision::TrustOnce,
                                    ));
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("save-host-key")
                                .primary()
                                .label("信任并保存")
                                .on_click(move |_, window, cx| {
                                    let _ = sender_save.send(SftpCommand::ResolveHostKey(
                                        HostKeyDecision::TrustAndSave,
                                    ));
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn open_upload_conflict(
        conflict: UploadConflict,
        sender: flume::Sender<SftpCommand>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let draft = cx.new(|_| ConflictDraft {
            apply_to_all: false,
        });
        window.open_dialog(cx, move |dialog, _, cx| {
            let apply_to_all = draft.read(cx).apply_to_all;
            let can_resume = conflict.can_resume();
            let conflict_id = conflict.id;
            let name = conflict.file_name();
            let sender_skip = sender.clone();
            let sender_overwrite = sender.clone();
            let sender_resume = sender.clone();
            let draft_skip = draft.clone();
            let draft_overwrite = draft.clone();
            let draft_resume = draft.clone();
            dialog
                .title(format!("处理“{name}”"))
                .close_button(false)
                .overlay_closable(false)
                .keyboard(false)
                .child(
                    v_flex()
                        .gap_2()
                        .child(format!("本地：{}", format_size(conflict.local_size)))
                        .child(format!(
                            "远端：{}",
                            conflict
                                .existing_size
                                .map(format_size)
                                .unwrap_or_else(|| "不存在".to_string())
                        ))
                        .child(format!(
                            "未完成：{}",
                            conflict
                                .filepart_size
                                .map(format_size)
                                .unwrap_or_else(|| "无".to_string())
                        ))
                        .child(
                            Checkbox::new("apply-conflict-to-all")
                                .label("对后续冲突执行相同操作")
                                .checked(apply_to_all)
                                .on_change({
                                    let draft = draft.clone();
                                    move |checked, _, cx| {
                                        draft.update(cx, |draft, cx| {
                                            draft.apply_to_all = *checked;
                                            cx.notify();
                                        });
                                    }
                                }),
                        ),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("skip-conflict").label("跳过").on_click(
                            move |_, window, cx| {
                                let _ = sender_skip.send(SftpCommand::ResolveUpload {
                                    id: conflict_id,
                                    decision: UploadDecision::Skip,
                                    apply_to_all: draft_skip.read(cx).apply_to_all,
                                });
                                window.close_dialog(cx);
                            },
                        ))
                        .child(
                            Button::new("overwrite-conflict")
                                .label("安全覆盖")
                                .on_click(move |_, window, cx| {
                                    let _ = sender_overwrite.send(SftpCommand::ResolveUpload {
                                        id: conflict_id,
                                        decision: UploadDecision::Overwrite,
                                        apply_to_all: draft_overwrite.read(cx).apply_to_all,
                                    });
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("resume-conflict")
                                .primary()
                                .disabled(!can_resume)
                                .label("断点续传")
                                .on_click(move |_, window, cx| {
                                    let _ = sender_resume.send(SftpCommand::ResolveUpload {
                                        id: conflict_id,
                                        decision: UploadDecision::Resume,
                                        apply_to_all: draft_resume.read(cx).apply_to_all,
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn open_text_editor(
        path: String,
        content: String,
        sender: flume::Sender<SftpCommand>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let editor = cx.new(|cx| {
            TextareaState::new(window, cx)
                .default_value(content)
                .placeholder("文件内容")
        });
        cx.subscribe(&editor, |_, event: &InputEvent, _| match event {
            InputEvent::Focus => gpui_mobile::show_keyboard(),
            InputEvent::Blur => gpui_mobile::hide_keyboard(),
            _ => {}
        })
        .detach();
        window.open_dialog(cx, move |dialog, _, _| {
            let sender = sender.clone();
            let editor_for_save = editor.clone();
            let path_for_save = path.clone();
            dialog
                .title(path.clone())
                .child(Textarea::new(&editor).h_96())
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text("保存")
                        .on_ok(move |_, _, cx| {
                            let _ = sender.send(SftpCommand::WriteText {
                                path: path_for_save.clone(),
                                content: editor_for_save.read(cx).value().to_string(),
                            });
                            gpui_mobile::hide_keyboard();
                            true
                        }),
                )
                .footer(Self::dialog_footer("text-editor-submit", "保存", true))
        });
    }

    fn open_entry_actions(
        app: WeakEntity<Self>,
        entry: FileEntry,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title = entry.name.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let app_download = app.clone();
            let app_edit = app.clone();
            let app_rename = app.clone();
            let app_delete = app.clone();
            let download_entry = entry.clone();
            let edit_path = entry.path.clone();
            let rename_entry = entry.clone();
            let delete_entry = entry.clone();
            dialog.title(title.clone()).child(
                v_flex()
                    .gap_2()
                    .child(
                        Button::new("download-entry")
                            .label(if entry.is_dir() {
                                "下载文件夹"
                            } else {
                                "下载"
                            })
                            .on_click(move |_, window, cx| {
                                let _ = app_download
                                    .update(cx, |app, cx| app.download(download_entry.clone(), cx));
                                window.close_dialog(cx);
                            }),
                    )
                    .when(!entry.is_dir(), |this| {
                        this.child(Button::new("edit-entry").label("打开并编辑文本").on_click(
                            move |_, window, cx| {
                                let _ = app_edit.update(cx, |app, _| {
                                    app.worker.send(SftpCommand::ReadText {
                                        path: edit_path.clone(),
                                    })
                                });
                                window.close_dialog(cx);
                            },
                        ))
                    })
                    .child(Button::new("rename-entry").label("重命名…").on_click(
                        move |_, window, cx| {
                            window.close_dialog(cx);
                            Self::open_rename_dialog(
                                app_rename.clone(),
                                rename_entry.clone(),
                                window,
                                cx,
                            );
                        },
                    ))
                    .child(
                        Button::new("delete-entry")
                            .danger()
                            .label("删除…")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                Self::open_delete_dialog(
                                    app_delete.clone(),
                                    delete_entry.clone(),
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
        });
    }

    fn open_rename_dialog(
        app: WeakEntity<Self>,
        entry: FileEntry,
        window: &mut Window,
        cx: &mut App,
    ) {
        let name = cx.new(|cx| InputState::new(window, cx).default_value(entry.name.clone()));
        window.open_dialog(cx, move |dialog, _, _| {
            let app = app.clone();
            let name_for_save = name.clone();
            let from = entry.path.clone();
            let parent = remote_path::parent(&from).unwrap_or_else(|| "/".to_string());
            dialog
                .title(format!("重命名“{}”", entry.name))
                .child(Input::new(&name))
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text("重命名")
                        .on_ok(move |_, _, cx| {
                            let new_name = name_for_save.read(cx).value().trim().to_string();
                            if new_name.is_empty() || new_name.contains('/') {
                                return false;
                            }
                            let to = remote_path::join(&parent, &new_name);
                            let _ = app.update(cx, |app, _| {
                                app.worker.send(SftpCommand::Rename {
                                    from: from.clone(),
                                    to,
                                });
                            });
                            true
                        }),
                )
                .footer(Self::dialog_footer("rename-submit", "重命名", true))
        });
    }

    fn open_delete_dialog(
        app: WeakEntity<Self>,
        entry: FileEntry,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app = app.clone();
            let path = entry.path.clone();
            let is_dir = entry.is_dir();
            alert
                .title(format!("删除“{}”？", entry.name))
                .description("远端删除无法从系统回收站恢复。")
                .confirm()
                .ok_text("删除")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let _ = app.update(cx, |app, _| {
                        app.worker.send(SftpCommand::Remove {
                            entries: vec![(path.clone(), is_dir)],
                        })
                    });
                    true
                })
        });
    }

    fn open_new_folder_dialog(app: WeakEntity<Self>, window: &mut Window, cx: &mut App) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("文件夹名称"));
        window.open_dialog(cx, move |dialog, _, _| {
            let app = app.clone();
            let name = name.clone();
            dialog
                .title("新建文件夹")
                .child(Input::new(&name))
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text("创建")
                        .on_ok(move |_, _, cx| {
                            let name = name.read(cx).value().trim().to_string();
                            if name.is_empty() || name.contains('/') {
                                return false;
                            }
                            let _ = app.update(cx, |app, _| {
                                app.worker.send(SftpCommand::Mkdir {
                                    path: remote_path::join(&app.remote_path, &name),
                                })
                            });
                            true
                        }),
                )
                .footer(Self::dialog_footer("new-folder-submit", "创建", true))
        });
    }

    fn open_file_actions(app: WeakEntity<Self>, window: &mut Window, cx: &mut App) {
        window.open_dialog(cx, move |dialog, _, _| {
            let upload_files = app.clone();
            let upload_folder = app.clone();
            let create_folder = app.clone();
            dialog.title("文件操作").child(
                v_flex()
                    .gap_2()
                    .child(
                        Button::new("upload-files")
                            .primary()
                            .label("上传文件…")
                            .on_click(move |_, window, cx| {
                                let _ = upload_files.update(cx, |app, cx| app.pick_uploads(cx));
                                window.close_dialog(cx);
                            }),
                    )
                    .child(Button::new("upload-folder").label("上传文件夹…").on_click(
                        move |_, window, cx| {
                            let _ =
                                upload_folder.update(cx, |app, cx| app.pick_upload_directory(cx));
                            window.close_dialog(cx);
                        },
                    ))
                    .child(Button::new("new-folder").label("新建远端文件夹…").on_click(
                        move |_, window, cx| {
                            window.close_dialog(cx);
                            Self::open_new_folder_dialog(create_folder.clone(), window, cx);
                        },
                    )),
            )
        });
    }

    fn open_settings_dialog(app: WeakEntity<Self>, window: &mut Window, cx: &mut App) {
        let Some(app_entity) = app.upgrade() else {
            return;
        };
        let settings = app_entity.read(cx).settings.clone();
        let discovery_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("发现用户名（可选）")
                .default_value(settings.discovery_username.clone().unwrap_or_default())
        });
        let discovery_port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("UDP 发现端口")
                .default_value(settings.discovery_port.to_string())
        });
        let http_port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("HTTP 端口")
                .default_value(settings.lan_http_port.to_string())
        });
        let lan_username = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("共享登录用户名")
                .default_value(settings.lan_sftp_username.clone())
        });
        let lan_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("共享登录密码")
                .masked(true)
        });
        let draft = cx.new(|_| SettingsDraft {
            manual_password: settings.lan_sftp_password_mode == LanSftpPasswordMode::Manual,
            error: None,
        });
        window.open_dialog(cx, move |dialog, _, cx| {
            let snapshot = draft.read(cx);
            let manual_password = snapshot.manual_password;
            let error = snapshot.error.clone();
            let app = app.clone();
            let draft_for_save = draft.clone();
            let discovery_name_for_save = discovery_name.clone();
            let discovery_port_for_save = discovery_port.clone();
            let http_port_for_save = http_port.clone();
            let lan_username_for_save = lan_username.clone();
            let lan_password_for_save = lan_password.clone();
            dialog
                .title("网络与共享")
                .child(
                    v_flex()
                        .gap_3()
                        .child(Input::new(&discovery_name))
                        .child(Input::new(&discovery_port))
                        .child(Input::new(&http_port))
                        .child(Input::new(&lan_username))
                        .child(
                            Switch::new("manual-lan-password")
                                .label("使用手动共享密码")
                                .checked(manual_password)
                                .on_change({
                                    let draft = draft.clone();
                                    move |checked, _, cx| {
                                        draft.update(cx, |draft, cx| {
                                            draft.manual_password = *checked;
                                            cx.notify();
                                        })
                                    }
                                }),
                        )
                        .when(manual_password, |this| {
                            this.child(Input::new(&lan_password).mask_toggle())
                        })
                        .when_some(error, |this, error| {
                            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                        }),
                )
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text("保存")
                        .on_ok(move |_, _, cx| {
                            let port = Self::input_value(&discovery_port_for_save, cx)
                                .parse::<u16>()
                                .ok()
                                .filter(|port| *port > 0);
                            let http_port = Self::input_value(&http_port_for_save, cx)
                                .parse::<u16>()
                                .ok()
                                .filter(|port| *port > 0);
                            let manual = draft_for_save.read(cx).manual_password;
                            let password = Self::input_value(&lan_password_for_save, cx);
                            let lan_username = Self::input_value(&lan_username_for_save, cx)
                                .trim()
                                .to_string();
                            if port.is_none()
                                || http_port.is_none()
                                || lan_username.is_empty()
                                || lan_username.chars().any(char::is_whitespace)
                                || (manual && password.is_empty())
                            {
                                draft_for_save.update(cx, |draft, cx| {
                                    draft.error = Some(
                                        if port.is_none() {
                                            "发现端口必须是 1–65535 之间的整数"
                                        } else if http_port.is_none() {
                                            "HTTP 端口必须是 1–65535 之间的整数"
                                        } else if lan_username.is_empty() {
                                            "共享登录用户名不能为空"
                                        } else if lan_username.chars().any(char::is_whitespace) {
                                            "共享登录用户名不能包含空白字符"
                                        } else {
                                            "手动密码不能为空"
                                        }
                                        .into(),
                                    );
                                    cx.notify();
                                });
                                return false;
                            }
                            let username = Self::input_value(&discovery_name_for_save, cx);
                            let _ = app.update(cx, |app, cx| {
                                app.settings.discovery_username =
                                    (!username.is_empty()).then_some(username.clone());
                                app.settings.discovery_port = port.expect("validated port");
                                app.settings.lan_http_port =
                                    http_port.expect("validated HTTP port");
                                app.settings.lan_sftp_username = lan_username.clone();
                                app.settings.lan_sftp_password_mode = if manual {
                                    LanSftpPasswordMode::Manual
                                } else {
                                    LanSftpPasswordMode::Default
                                };
                                let saved_password = manual.then_some(password.as_str());
                                match store::save_settings_with_lan_password(
                                    &app.settings,
                                    saved_password,
                                ) {
                                    Ok(()) => {
                                        if let Some(discovery) = &app.discovery {
                                            let _ = discovery.set_username(
                                                app.settings.discovery_username.as_deref(),
                                            );
                                            let _ = discovery
                                                .set_discovery_port(app.settings.discovery_port);
                                        }
                                        if let Ok(server) = &app.lan_server {
                                            let _ = server.set_username(&lan_username);
                                            let _ = server
                                                .set_password(manual.then_some(password.clone()));
                                        }
                                        app.status_message = Some(
                                            match app.apply_http_settings(
                                                manual.then_some(password.clone()),
                                            ) {
                                                Ok(()) => "设置已保存".to_string(),
                                                Err(message) => format!(
                                                    "设置已保存，但 HTTP 服务未切换：{message}"
                                                ),
                                            }
                                            .into(),
                                        );
                                    }
                                    Err(message) => app.status_message = Some(message.into()),
                                }
                                cx.notify();
                            });
                            true
                        }),
                )
                .footer(Self::dialog_footer("network-settings-submit", "保存", true))
        });
    }

    fn choose_lan_root(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            let selected = cx
                .background_executor()
                .spawn(async {
                    #[cfg(target_os = "ios")]
                    {
                        gpui_mobile::packages::file_selector::get_directory_path(None)
                    }
                    #[cfg(target_os = "android")]
                    {
                        crate::android_picker::pick_shared_directory()
                    }
                })
                .await;
            let _ = this.update(cx, |app, cx| match selected {
                Ok(Some(path)) => {
                    let path = PathBuf::from(path);
                    let password = match app.settings.lan_sftp_password_mode {
                        LanSftpPasswordMode::Default => None,
                        LanSftpPasswordMode::Manual => {
                            store::load_lan_sftp_password().ok().flatten()
                        }
                    };
                    let new_server = match &app.lan_server {
                        Ok(server) => match server.set_root(path.clone()) {
                            Ok(()) => None,
                            Err(message) => {
                                app.status_message = Some(message.into());
                                cx.notify();
                                return;
                            }
                        },
                        Err(_) => {
                            match crate::sftp::lan_server::start(
                                Some(path.clone()),
                                &app.settings.lan_sftp_username,
                                password.clone(),
                                app.lan_activity.clone(),
                            ) {
                                Ok(server) => Some(server),
                                Err(message) => {
                                    app.status_message = Some(message.into());
                                    cx.notify();
                                    return;
                                }
                            }
                        }
                    };
                    if let Some(server) = new_server {
                        let port = server.port;
                        app.lan_server = Ok(server);
                        if let Some(discovery) = &app.discovery {
                            let _ = discovery.set_port(Some(port));
                        }
                    }
                    let mut new_http_server = None;
                    let http_start_error = match &app.http_server {
                        Ok(server) => match server.set_root(path.clone()) {
                            Ok(()) => None,
                            Err(message) => {
                                app.status_message = Some(message.into());
                                cx.notify();
                                return;
                            }
                        },
                        Err(_) => match crate::http_share::start(
                            Some(path.clone()),
                            &app.settings.lan_sftp_username,
                            password,
                            app.settings.lan_http_port,
                            app.lan_activity.clone(),
                        ) {
                            Ok(server) => {
                                new_http_server = Some(server);
                                None
                            }
                            Err(message) => Some(message),
                        },
                    };
                    if let Some(server) = new_http_server {
                        app.http_server = Ok(server);
                    } else if let Some(message) = &http_start_error {
                        app.http_server = Err(message.clone().into());
                    }
                    app.refresh_http_qr_code();
                    app.settings.lan_sftp_root = Some(path);
                    if let Err(message) =
                        store::save_settings_with_lan_password(&app.settings, None)
                    {
                        app.status_message = Some(message.into());
                    } else if let Some(message) = http_start_error {
                        app.status_message =
                            Some(format!("共享目录已切换，但 HTTP 服务启动失败：{message}").into());
                    } else {
                        app.status_message = Some("共享目录已切换".into());
                    }
                    cx.notify();
                }
                Ok(None) => {}
                Err(message) => app.status_message = Some(message.into()),
            });
        });
        self._background_tasks.push(task);
    }

    fn render_files(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if matches!(
            self.connection,
            ConnectionState::Connecting | ConnectionState::Reconnecting
        ) {
            let title = if self.connection == ConnectionState::Connecting {
                "正在连接…"
            } else {
                "连接中断，正在重连…"
            };
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_6()
                .child(Spinner::new().with_size(px(32.)).color(cx.theme().primary))
                .child(div().text_xl().child(title))
                .when_some(self.endpoint.clone(), |this, endpoint| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("正在连接到 {endpoint}")),
                    )
                })
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("首次连接或网络较慢时可能需要几秒"),
                )
                .into_any_element();
        }

        if self.connection == ConnectionState::Disconnected {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_6()
                .child(div().text_xl().child("连接后浏览远端文件"))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("移动端使用单栏目录导航；上传由系统文件选择器提供本地文件。"),
                )
                .child(
                    Button::new("open-connect")
                        .primary()
                        .label("连接 SFTP…")
                        .disabled(!self.unlocked)
                        .on_click({
                            let app = cx.entity().downgrade();
                            move |_, window, cx| {
                                Self::open_connection_dialog(app.clone(), None, window, cx)
                            }
                        }),
                )
                .into_any_element();
        }

        let parent = remote_path::parent(&self.remote_path);
        if self.entries.is_empty() {
            return v_flex()
                .size_full()
                .when_some(parent, |this, parent| {
                    this.child(self.render_parent_directory(parent, cx))
                })
                .child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .p_6()
                        .child(if self.remote_loading {
                            "正在读取目录…"
                        } else if self.remote_error.is_some() {
                            "无法读取目录"
                        } else {
                            "此文件夹为空"
                        })
                        .when_some(self.remote_error.clone(), |this, error| {
                            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                        }),
                )
                .into_any_element();
        }

        let files = uniform_list("remote-files", self.entries.len(), {
            cx.processor(|app, visible: Range<usize>, _window, cx| {
                visible
                    .map(|index| {
                        let entry = app.entries[index].clone();
                        let is_dir = entry.is_dir();
                        let leading_icon: &'static [u8] = match entry.kind {
                            EntryKind::Directory => ICON_FOLDER,
                            EntryKind::Symlink => ICON_LINK,
                            EntryKind::File => ICON_FILE,
                        };
                        let subtitle = match entry.kind {
                            EntryKind::Directory => "文件夹".to_string(),
                            EntryKind::Symlink => "符号链接".to_string(),
                            EntryKind::File => format_size(entry.size),
                        };
                        let path = entry.path.clone();
                        let app_entity = cx.entity().downgrade();
                        ListTile::new(MaterialTheme::from_appearance(app.dark_mode))
                            .id(format!("remote-entry:{path}"))
                            .leading(
                                div().w_6().flex().items_center().justify_center().child(
                                    Icon::default()
                                        .data(leading_icon)
                                        .small()
                                        .text_color(cx.theme().muted_foreground),
                                ),
                            )
                            .title(entry.name.clone())
                            .subtitle(subtitle)
                            .trailing(
                                Icon::default()
                                    .data(if is_dir {
                                        ICON_CHEVRON_RIGHT
                                    } else {
                                        ICON_ELLIPSIS_VERTICAL
                                    })
                                    .small()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .on_click(cx.listener(move |app, _, window, cx| {
                                if is_dir {
                                    app.request_directory(path.clone(), None, cx);
                                } else {
                                    Self::open_entry_actions(
                                        app_entity.clone(),
                                        entry.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            }))
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
        })
        .size_full()
        .track_scroll(&self.remote_scroll);
        v_flex()
            .size_full()
            .when_some(parent, |this, parent| {
                this.child(self.render_parent_directory(parent, cx))
            })
            .child(div().flex_1().min_h_0().child(files))
            .into_any_element()
    }

    fn render_parent_directory(&self, parent: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        ListTile::new(MaterialTheme::from_appearance(self.dark_mode))
            .id("remote-parent-directory")
            .leading(
                div().w_6().flex().items_center().justify_center().child(
                    Icon::default()
                        .data(ICON_CORNER_LEFT_UP)
                        .small()
                        .text_color(cx.theme().muted_foreground),
                ),
            )
            .title("上级目录")
            .subtitle(parent.clone())
            .trailing(
                Icon::default()
                    .data(ICON_CHEVRON_RIGHT)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .on_click(
                cx.listener(move |app, _, _, cx| app.request_directory(parent.clone(), None, cx)),
            )
            .into_any_element()
    }

    fn render_transfers(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.transfers.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child("暂无传输")
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("在“文件”中上传或下载后，任务会显示在这里。"),
                )
                .into_any_element();
        }
        let has_clearable = self.transfers.iter().any(|row| row.state.is_clearable());
        let rows = self.transfers.clone();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .justify_end()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("clear-completed-transfers")
                            .small()
                            .label("清除已完成")
                            .disabled(!has_clearable)
                            .on_click(cx.listener(|app, _, _, cx| {
                                app.transfers.retain(|row| !row.state.is_clearable());
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("transfer-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows.into_iter().map(|row| {
                        let active = row.state.is_active();
                        let id = row.id;
                        let total = row.total;
                        let ratio = row.ratio();
                        let direction = match row.direction {
                            TransferDirection::Upload => "↑",
                            TransferDirection::Download => "↓",
                        };
                        let detail = if row.total_files > 0 {
                            format!(
                                "{}/{} 个文件 · {}",
                                row.completed_files,
                                row.total_files,
                                row.state.label()
                            )
                        } else if row.speed > 0. {
                            format!(
                                "{} · {}/s",
                                row.state.label(),
                                format_size(row.speed as u64)
                            )
                        } else {
                            row.state.label()
                        };
                        v_flex()
                            .p_4()
                            .gap_2()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(
                                h_flex()
                                    .justify_between()
                                    .gap_3()
                                    .child(
                                        v_flex()
                                            .min_w_0()
                                            .child(format!("{direction} {}", row.name))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(detail),
                                            ),
                                    )
                                    .child(if active {
                                        Button::new(("cancel-transfer", id as usize))
                                            .small()
                                            .danger()
                                            .label("取消")
                                            .on_click(cx.listener(move |app, _, _, _| {
                                                app.worker.send(SftpCommand::Cancel(id));
                                            }))
                                    } else {
                                        Button::new(("delete-transfer", id as usize))
                                            .small()
                                            .danger()
                                            .label("删除")
                                            .on_click(cx.listener(move |app, _, _, cx| {
                                                app.transfers.retain(|row| row.id != id);
                                                cx.notify();
                                            }))
                                    }),
                            )
                            .when(total > 0, |this| {
                                this.child(
                                    Progress::new(("transfer-progress", id as usize))
                                        .value(ratio * 100.),
                                )
                            })
                    })),
            )
            .into_any_element()
    }

    fn render_connections(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let app = cx.entity().downgrade();
        let mut content = v_flex()
            .id("connection-list")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_5()
            .child(
                h_flex()
                    .justify_between()
                    .child(v_flex().child(self.connection.label()).when_some(
                        self.endpoint.clone(),
                        |this, endpoint| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(endpoint),
                            )
                        },
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .when(self.connection == ConnectionState::Connected, |this| {
                                this.child(
                                    Button::new("install-public-key")
                                        .label("部署公钥…")
                                        .on_click(cx.listener(|app, _, _, cx| {
                                            app.pick_and_install_public_key(cx)
                                        })),
                                )
                            })
                            .child(if self.connection == ConnectionState::Disconnected {
                                Button::new("new-connection")
                                    .primary()
                                    .label("新连接…")
                                    .disabled(!self.unlocked)
                                    .on_click({
                                        let app = app.clone();
                                        move |_, window, cx| {
                                            Self::open_connection_dialog(
                                                app.clone(),
                                                None,
                                                window,
                                                cx,
                                            )
                                        }
                                    })
                                    .into_any_element()
                            } else {
                                Button::new("disconnect")
                                    .danger()
                                    .label("断开")
                                    .on_click(cx.listener(|app, _, _, cx| app.disconnect(cx)))
                                    .into_any_element()
                            }),
                    ),
            );

        let discovered = self.discovered.clone();
        if !discovered.is_empty() {
            content = content.child(div().text_sm().child("附近的 Blip"));
            for device in discovered {
                let app = app.clone();
                content = content.child(
                    ListTile::new(MaterialTheme::from_appearance(self.dark_mode))
                        .id(format!("discovered-device:{}", device.id))
                        .leading_icon("◉")
                        .title(device.display_name())
                        .subtitle(device.display_label())
                        .trailing_icon("›")
                        .on_click(move |_, window, cx| {
                            if let Some(app_entity) = app.upgrade() {
                                let (host, port, username) = {
                                    let app_ref = app_entity.read(cx);
                                    (
                                        app_ref.host.clone(),
                                        app_ref.port.clone(),
                                        app_ref.username.clone(),
                                    )
                                };
                                Self::set_input(&host, device.address.to_string(), window, cx);
                                Self::set_input(&port, device.port.to_string(), window, cx);
                                Self::set_input(&username, "zy", window, cx);
                            }
                            Self::open_connection_dialog(app.clone(), None, window, cx);
                        }),
                );
            }
        }

        content = content.child(div().text_sm().child("收藏与最近连接"));
        if self.profiles.is_empty() {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("连接成功后会自动保存在加密配置中。"),
            );
        } else {
            for profile in self.profiles.clone() {
                let app = app.clone();
                let profile_id = profile.endpoint_label();
                content = content.child(
                    ListTile::new(MaterialTheme::from_appearance(self.dark_mode))
                        .id(format!("saved-profile:{profile_id}"))
                        .leading_icon(if profile.pinned { "★" } else { "🔐" })
                        .title(profile.title())
                        .subtitle(match profile.auth {
                            SavedAuth::Password => "密码认证",
                            SavedAuth::Key { .. } => "私钥认证",
                        })
                        .trailing_icon("⋯")
                        .on_click(move |_, window, cx| {
                            Self::open_profile_actions(app.clone(), profile.clone(), window, cx)
                        }),
                );
            }
        }
        content.into_any_element()
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let app = cx.entity().downgrade();
        let server_info = match &self.lan_server {
            Ok(server) => (
                Some(server.endpoint()),
                Some(server.command()),
                Some(server.root().display().to_string()),
                None,
                Some(server.password()),
            ),
            Err(message) => (None, None, None, Some(message.clone()), None),
        };
        let (server_endpoint, server_command, server_root, server_error, password) = server_info;
        let password_to_copy = password.clone();
        let (http_url, http_error) = match &self.http_server {
            Ok(server) => (Some(server.url()), None),
            Err(message) => (None, Some(message.clone())),
        };
        let http_qr_code = self.http_qr_code.clone();
        let qr_unavailable = http_url.is_some() && http_qr_code.is_none();
        let http_url_to_copy = http_url.clone();
        v_flex()
            .id("settings-list")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .child(div().text_sm().child("局域网接收"))
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .p_4()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .items_start()
                            .gap_3()
                            .child(div().text_xl().child("📡"))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(div().text_base().child("SFTP 接收"))
                                    .when_some(server_endpoint, |this, endpoint| {
                                        this.child(
                                            div()
                                                .text_sm()
                                                .whitespace_normal()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!("地址：{endpoint}")),
                                        )
                                    })
                                    .when_some(server_command, |this, command| {
                                        this.child(
                                            div()
                                                .text_sm()
                                                .whitespace_normal()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!("命令：{command}")),
                                        )
                                    })
                                    .when_some(server_root, |this, root| {
                                        this.child(
                                            div()
                                                .text_sm()
                                                .whitespace_normal()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!("共享目录：{root}")),
                                        )
                                    })
                                    .when_some(server_error, |this, error| {
                                        this.child(
                                            div()
                                                .text_sm()
                                                .whitespace_normal()
                                                .text_color(cx.theme().danger)
                                                .child(error),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        Button::new("copy-lan-password")
                            .label("复制登录密码")
                            .disabled(password.is_none())
                            .on_click(move |_, window, cx| {
                                if let Some(password) = &password_to_copy {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        password.clone(),
                                    ));
                                    window.push_notification("登录密码已复制", cx);
                                }
                            }),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .p_4()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(div().text_base().child("HTTP 文件共享"))
                    .when_some(http_url, |this, url| {
                        this.child(
                            div()
                                .text_sm()
                                .whitespace_normal()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("访问地址：{url}")),
                        )
                    })
                    .when_some(http_qr_code, |this, (image, size)| {
                        this.child(
                            v_flex()
                                .items_center()
                                .gap_2()
                                .child(img(image).size(px(size)))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("扫码打开此地址"),
                                ),
                        )
                    })
                    .when(qr_unavailable, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("二维码生成失败，可复制访问地址"),
                        )
                    })
                    .when_some(http_error, |this, error| {
                        this.child(
                            div()
                                .text_sm()
                                .whitespace_normal()
                                .text_color(cx.theme().danger)
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("与 SFTP 共用共享目录、用户名和密码"),
                    )
                    .child(
                        Button::new("copy-http-url")
                            .label("复制访问地址")
                            .disabled(http_url_to_copy.is_none())
                            .on_click(move |_, window, cx| {
                                if let Some(url) = &http_url_to_copy {
                                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                                    window.push_notification("访问地址已复制", cx);
                                }
                            }),
                    ),
            )
            .child(
                Button::new("choose-share-root")
                    .label("选择共享目录…")
                    .disabled(!self.unlocked)
                    .on_click(cx.listener(|app, _, _, cx| app.choose_lan_root(cx))),
            )
            .child(
                Button::new("network-settings")
                    .label("网络与共享设置…")
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| Self::open_settings_dialog(app.clone(), window, cx)
                    }),
            )
            .child(div().text_sm().child("外观"))
            .child(
                Switch::new("dark-mode")
                    .label("深色模式")
                    .checked(self.dark_mode)
                    .on_change(cx.listener(|app, checked, _, cx| {
                        app.dark_mode = *checked;
                        gpui_kit::component::Theme::change(
                            if *checked {
                                gpui_kit::component::ThemeMode::Dark
                            } else {
                                gpui_kit::component::ThemeMode::Light
                            },
                            None,
                            cx,
                        );
                        cx.notify();
                    })),
            )
            .child(div().text_sm().child("关于"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "Blip Mobile {} · GPUI Mobile",
                        env!("CARGO_PKG_VERSION")
                    )),
            )
            .when(self.available_update.is_some(), |this| {
                this.child(
                    Button::new("available-update")
                        .primary()
                        .label("有新版本可用")
                        .on_click(
                            cx.listener(|app, _, window, cx| app.open_update_dialog(window, cx)),
                        ),
                )
            })
            .when_some(self.status_message.clone(), |this, message| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(message),
                )
            })
            .into_any_element()
    }

    fn render_top_bar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = MaterialTheme::from_appearance(self.dark_mode);
        let title = match self.screen {
            Screen::Files if self.connection == ConnectionState::Connected => {
                self.remote_path.clone()
            }
            Screen::Files => "文件".to_string(),
            Screen::Transfers => "传输".to_string(),
            Screen::Connections => "连接".to_string(),
            Screen::Settings => "设置".to_string(),
        };
        let mut bar = TopAppBar::small(title, theme);
        if self.screen == Screen::Files
            && self.connection == ConnectionState::Connected
            && remote_path::parent(&self.remote_path).is_some()
        {
            bar = bar.leading_icon("←", cx.listener(|app, _, _, cx| app.go_up(cx)));
        }
        if self.screen == Screen::Files && self.connection == ConnectionState::Connected {
            let app = cx.entity().downgrade();
            bar = bar
                .trailing_icon(
                    "刷新",
                    cx.listener(|app, _, _, cx| app.refresh_directory(cx)),
                )
                .trailing_icon("＋", move |_, window, cx| {
                    Self::open_file_actions(app.clone(), window, cx)
                });
        }
        bar.into_any_element()
    }

    fn render_navigation(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let current = self.screen;
        NavigationBarBuilder::new(self.dark_mode)
            .item(
                "◉",
                "连接",
                current == Screen::Connections,
                cx.listener(|app, _, _, cx| {
                    app.screen = Screen::Connections;
                    cx.notify();
                }),
            )
            .item(
                "📁",
                "文件",
                current == Screen::Files,
                cx.listener(|app, _, _, cx| {
                    app.screen = Screen::Files;
                    cx.notify();
                }),
            )
            .item(
                "⇅",
                "传输",
                current == Screen::Transfers,
                cx.listener(|app, _, _, cx| {
                    app.screen = Screen::Transfers;
                    cx.notify();
                }),
            )
            .item(
                "⚙",
                "设置",
                current == Screen::Settings,
                cx.listener(|app, _, _, cx| {
                    app.screen = Screen::Settings;
                    cx.notify();
                }),
            )
            .build()
            .into_any_element()
    }
}

fn safe_area_insets() -> (f32, f32, f32, f32) {
    #[cfg(target_os = "android")]
    {
        use jni::objects::{JFloatArray, JValue};

        gpui_mobile::android::jni::with_env(|env| {
            let activity = gpui_mobile::android::jni::activity(env)?;
            let class = gpui_mobile::android::jni::find_app_class(
                env,
                "com.amov.blip.mobile.BlipActivity",
            )?;
            let result = env
                .call_static_method(
                    &class,
                    jni::jni_str!("getSafeAreaInsets"),
                    jni::jni_sig!("(Landroid/app/Activity;)[F"),
                    &[JValue::Object(&activity)],
                )
                .and_then(|value| value.l())
                .map_err(|error| {
                    env.exception_clear();
                    error.to_string()
                })?;
            let array = JFloatArray::cast_local(env, result).map_err(|error| error.to_string())?;
            if array.len(env).map_err(|error| error.to_string())? != 4 {
                return Err("Android safe-area response has an invalid length".to_string());
            }
            let mut insets = [0.0; 4];
            array
                .get_region(env, 0, &mut insets)
                .map_err(|error| error.to_string())?;
            Ok((insets[0], insets[1], insets[2], insets[3]))
        })
        .unwrap_or_default()
    }

    #[cfg(target_os = "ios")]
    {
        gpui_mobile::safe_area_insets()
    }
}

impl Render for MobileApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (safe_top, safe_bottom, safe_left, safe_right) = safe_area_insets();
        let material = MaterialTheme::from_appearance(self.dark_mode);
        set_system_chrome(&SystemChromeStyle {
            status_bar_color: Some(material.surface),
            status_bar_style: if self.dark_mode {
                StatusBarContentStyle::Light
            } else {
                StatusBarContentStyle::Dark
            },
            navigation_bar_color: Some(material.surface_container),
        });
        let content = match self.screen {
            Screen::Files => self.render_files(cx),
            Screen::Transfers => self.render_transfers(cx),
            Screen::Connections => self.render_connections(cx),
            Screen::Settings => self.render_settings(cx),
        };

        v_flex()
            .size_full()
            .bg(rgb(material.surface))
            .text_color(rgb(material.on_surface))
            .on_key_down(cx.listener(|app, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && app.screen == Screen::Files
                    && remote_path::parent(&app.remote_path).is_some()
                {
                    app.go_up(cx);
                    cx.stop_propagation();
                }
            }))
            .when(safe_top > 0., |this| this.child(div().h(px(safe_top))))
            .child(self.render_top_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .pl(px(safe_left))
                    .pr(px(safe_right))
                    .child(content),
            )
            .child(self.render_navigation(cx))
            .when(safe_bottom > 0., |this| {
                this.child(div().h(px(safe_bottom)))
            })
    }
}
