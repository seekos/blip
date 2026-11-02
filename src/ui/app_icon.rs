//! 让裸二进制也带上自己的应用图标。
//!
//! `.app` bundle 由 `Info.plist` 的 `CFBundleIconFile` 指定图标，Finder / Dock
//! 会自动去读。但 `cargo run` 跑的是 `target/debug/macscp` 这个裸 Unix 可执行
//! 文件 —— 没有 bundle、没有 Info.plist，Dock 里就是那张通用的可执行文件图标。
//!
//! 这里把 `assets/icon/AppIcon.icns` 用 `include_bytes!` 编进二进制，启动后
//! 交给 `NSApplication.setApplicationIconImage:`。用内嵌字节而不是读磁盘路径，
//! 是因为裸二进制的 cwd 不可靠（从 Finder 拖进终端、从别的目录启动都会变）。
//!
//! 打包成 `.app` 之后这段代码照样执行，只是图标本来就对，等于重复设置一次 ——
//! 无害，也就不必区分两种启动方式。

#[cfg(target_os = "linux")]
use std::sync::{Arc, OnceLock};

#[cfg(target_os = "linux")]
use gpui_kit::{Image, ImageFormat};

#[cfg(target_os = "linux")]
const ICON_PNG: &[u8] = include_bytes!("../../docs/app-icon.png");

/// 供自绘标题栏复用的内嵌图标。
#[cfg(target_os = "linux")]
pub fn image() -> Arc<Image> {
    static IMAGE: OnceLock<Arc<Image>> = OnceLock::new();
    IMAGE
        .get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Png, ICON_PNG.to_vec())))
        .clone()
}

/// 把内嵌的 icns 装成当前进程的 Dock 图标。
///
/// 失败时静默返回：图标是纯装饰，不能因为它让应用起不来。
pub fn install() {
    imp::install();
}

#[cfg(target_os = "linux")]
pub fn linux_window_icon() -> Option<Arc<image::RgbaImage>> {
    match image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png) {
        Ok(icon) => Some(Arc::new(icon.into_rgba8())),
        Err(error) => {
            log::warn!("应用图标解析失败：{error}");
            None
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn embedded_linux_icons_are_valid() {
        assert!(super::linux_window_icon().is_some());
        assert!(!super::image().bytes.is_empty());
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::AnyThread as _;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    /// 构建时就把图标嵌进来：`AppIcon.icns` 由 `scripts/make-icon.sh` 生成。
    const ICON_ICNS: &[u8] = include_bytes!("../../assets/icon/AppIcon.icns");

    pub fn install() {
        // `MainThreadMarker::new()` 在非主线程返回 `None` —— 这种情况不该发生
        // （调用点在 `application().run` 的闭包里），但也没必要 panic。
        let Some(mtm) = MainThreadMarker::new() else {
            log::warn!("不在主线程，跳过 Dock 图标设置");
            return;
        };

        let data = NSData::with_bytes(ICON_ICNS);
        // icns 是 NSImage 原生支持的容器格式，不用先转 PNG。
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
            log::warn!("AppIcon.icns 解析失败，Dock 图标保持默认");
            return;
        };

        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: `image` 活得比这次调用长（`Retained`），AppKit 会自己持有它。
        unsafe { app.setApplicationIconImage(Some(&image)) };
        log::debug!("Dock 图标已设置为内嵌的 AppIcon.icns");
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn install() {}
}
