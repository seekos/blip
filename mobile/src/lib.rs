//! Blip mobile entry points.

#[cfg(target_os = "android")]
mod android_picker;
#[cfg(any(target_os = "ios", target_os = "android"))]
mod app;
#[cfg_attr(not(any(target_os = "ios", target_os = "android")), allow(dead_code))]
mod qr;
mod remote_path;

#[cfg(any(target_os = "ios", target_os = "android"))]
#[allow(dead_code)]
#[path = "../../src/discovery.rs"]
mod discovery;
#[cfg(any(target_os = "ios", target_os = "android"))]
#[allow(dead_code)]
#[path = "../../src/http_share.rs"]
mod http_share;
#[cfg(any(target_os = "ios", target_os = "android"))]
#[allow(dead_code)]
mod session;
#[cfg(any(target_os = "ios", target_os = "android"))]
// Mobile reuses only part of the desktop service API.
#[allow(dead_code, unused_imports)]
#[path = "../../src/sftp/mod.rs"]
mod sftp;
#[cfg(any(target_os = "ios", target_os = "android"))]
#[allow(dead_code)]
#[path = "../../src/update.rs"]
mod update;

#[cfg(target_os = "android")]
use gpui::Application;
#[cfg(any(target_os = "ios", target_os = "android"))]
use gpui::{App, WindowOptions};

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(android_app: android_activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("blip-mobile"),
    );
    gpui_mobile::android::jni::install_panic_hook();
    let _platform = gpui_mobile::android::jni::init_platform(&android_app);
    let Some(platform) = gpui_mobile::android::jni::shared_platform() else {
        log::error!("无法初始化 Android GPUI 平台");
        return;
    };
    Application::with_platform(platform.into_rc()).run(open_main_window);
}

#[cfg(target_os = "ios")]
struct IosLogger;

#[cfg(target_os = "ios")]
impl log::Log for IosLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

#[cfg(target_os = "ios")]
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_register_app() {
    static LOGGER: IosLogger = IosLogger;
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
    gpui_mobile::ios::ffi::set_app_callback(Box::new(open_main_window));
}

#[cfg(target_os = "ios")]
pub fn ios_main() {
    gpui_ios_register_app();
    gpui_mobile::ios::ffi::run_app();
}

#[cfg(any(target_os = "ios", target_os = "android"))]
fn open_main_window(cx: &mut App) {
    use gpui::AppContext as _;
    use gpui_kit::component::{Root, Theme, ThemeMode};

    if let Err(error) = initialize_mobile_home() {
        log::error!("无法初始化移动端应用目录：{error}");
    }
    gpui_kit::init(cx);
    Theme::change(ThemeMode::Dark, None, cx);
    match cx.open_window(WindowOptions::default(), |window, cx| {
        let view = cx.new(|cx| app::MobileApp::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    }) {
        Ok(_) => cx.activate(true),
        Err(error) => log::error!("无法打开 Blip 移动端窗口：{error:#}"),
    }
}

#[cfg(any(target_os = "ios", target_os = "android"))]
fn initialize_mobile_home() -> Result<(), String> {
    let home = gpui_mobile::packages::path_provider::support_directory()?;
    std::fs::create_dir_all(&home)
        .map_err(|error| format!("创建 {} 失败：{error}", home.display()))?;

    // The shared desktop services resolve config, keys and known_hosts from HOME.
    // Set it before Blip starts any workers so they all use the private app sandbox.
    unsafe { std::env::set_var("HOME", &home) };
    if dirs::home_dir().as_deref() != Some(home.as_path()) {
        return Err("应用私有目录没有生效".to_string());
    }
    log::info!("移动端应用目录已就绪：{}", home.display());
    Ok(())
}
