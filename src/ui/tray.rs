//! 系统托盘及主窗口显示/隐藏行为。

use anyhow::{Context as _, Result};
use gpui_kit::{AnyWindowHandle, App, AppContext, Global};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent, TrayIconId,
    menu::{Menu, MenuEvent, MenuItem},
};

const TRAY_ID: &str = "blip";
const SHOW_MAIN_MENU_ID: &str = "show-main-menu";
const QUIT_ID: &str = "quit";
const ICON_PNG: &[u8] = include_bytes!("../../docs/app-icon.png");

struct TrayState {
    _icon: TrayIcon,
    _minimize_handler: platform::MinimizeHandler,
}

impl Global for TrayState {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayCommand {
    ShowMainMenu,
    Quit,
}

fn command_for_menu_id(id: &str) -> Option<TrayCommand> {
    match id {
        SHOW_MAIN_MENU_ID => Some(TrayCommand::ShowMainMenu),
        QUIT_ID => Some(TrayCommand::Quit),
        _ => None,
    }
}

fn command_for_tray_click(
    id: &TrayIconId,
    button: MouseButton,
    button_state: MouseButtonState,
) -> Option<TrayCommand> {
    (id == TRAY_ID && button == MouseButton::Left && button_state == MouseButtonState::Up)
        .then_some(TrayCommand::ShowMainMenu)
}

pub(crate) fn show_main_window<C: AppContext>(main_window: AnyWindowHandle, cx: &mut C) {
    if let Err(err) = main_window.update(cx, |_, window, cx| {
        platform::show(window);
        window.activate_window();
        cx.activate(true);
    }) {
        log::warn!("无法显示主窗口：{err:#}");
    }
}

/// 安装托盘；成功后才接管最小化，确保主窗口始终有恢复入口。
pub fn install(main_window: AnyWindowHandle, cx: &mut App) -> Result<()> {
    let show = MenuItem::with_id(SHOW_MAIN_MENU_ID, "显示主菜单", true, None);
    let quit = MenuItem::with_id(QUIT_ID, "退出", true, None);
    let menu = Menu::with_items(&[&show, &quit]).context("无法创建托盘菜单")?;
    let icon = load_icon()?;
    let icon = TrayIconBuilder::new()
        .with_id(TRAY_ID)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Blip")
        .with_icon(icon)
        .build()
        .context("无法创建托盘图标")?;

    let (commands, command_events) = flume::unbounded();
    let menu_commands = commands.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(command) = command_for_menu_id(event.id().as_ref()) {
            let _ = menu_commands.send(command);
        }
    }));
    let tray_commands = commands.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if let TrayIconEvent::Click {
            id,
            button,
            button_state,
            ..
        } = event
            && let Some(command) = command_for_tray_click(&id, button, button_state)
        {
            let _ = tray_commands.send(command);
        }
    }));

    let minimize_handler = main_window
        .update(cx, |_, window, _| {
            platform::install_minimize_handler(window)
        })
        .context("无法访问主窗口")??;
    cx.set_global(TrayState {
        _icon: icon,
        _minimize_handler: minimize_handler,
    });

    cx.spawn(async move |cx| {
        while let Ok(command) = command_events.recv_async().await {
            match command {
                TrayCommand::ShowMainMenu => show_main_window(main_window, cx),
                TrayCommand::Quit => {
                    cx.update(|cx| cx.quit());
                    break;
                }
            }
        }
    })
    .detach();

    Ok(())
}

fn load_icon() -> Result<Icon> {
    let image = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png)
        .context("无法解析托盘图标")?
        .resize_exact(32, 32, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).context("托盘图标尺寸无效")
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Context as _, Result};
    use gpui_kit::Window;
    use objc2::{
        DefinedClass, MainThreadOnly, define_class, msg_send, rc::Retained, runtime::AnyObject, sel,
    };
    use objc2_app_kit::{NSView, NSWindow, NSWindowButton};
    use objc2_foundation::{MainThreadMarker, NSObject};
    use raw_window_handle::RawWindowHandle;

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = Retained<NSWindow>]
        struct MinimizeTarget;

        impl MinimizeTarget {
            #[unsafe(method(minimizeToTray:))]
            fn minimize_to_tray(&self, _sender: Option<&AnyObject>) {
                self.ivars().orderOut(None);
            }
        }
    );

    pub struct MinimizeHandler {
        _target: Retained<MinimizeTarget>,
    }

    pub fn install_minimize_handler(window: &Window) -> Result<MinimizeHandler> {
        let window = ns_window(window).context("无法取得 macOS 原生窗口")?;
        let button = window
            .standardWindowButton(NSWindowButton::MiniaturizeButton)
            .context("无法取得 macOS 最小化按钮")?;
        let mtm = MainThreadMarker::new().context("最小化按钮必须在主线程安装")?;
        let target = mtm.alloc().set_ivars(window);
        let target: Retained<MinimizeTarget> = unsafe { msg_send![super(target), init] };

        // NSControl 的 target 是弱引用，因此由 TrayState 持有 target 的生命周期。
        unsafe {
            button.setTarget(Some(&target));
            button.setAction(Some(sel!(minimizeToTray:)));
        }

        Ok(MinimizeHandler { _target: target })
    }

    pub fn show(_window: &Window) {}

    fn ns_window(window: &Window) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
        let handle = raw_window_handle::HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        let view = unsafe { (handle.ns_view.as_ptr() as *const NSView).as_ref()? };
        view.window()
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use anyhow::Result;
    use gpui_kit::Window;
    use gpui_kit::WindowVisibility;
    use raw_window_handle::RawWindowHandle;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, SW_HIDE, SW_RESTORE, ShowWindow};

    pub struct MinimizeHandler;

    pub fn install_minimize_handler(window: &Window) -> Result<MinimizeHandler> {
        window
            .observe_window_visibility(|visibility, window, _| {
                if visibility == WindowVisibility::Hidden {
                    hide_if_minimized(window);
                }
            })
            .detach();
        Ok(MinimizeHandler)
    }

    pub fn hide_if_minimized(window: &Window) {
        let Some(hwnd) = hwnd(window) else {
            return;
        };
        if unsafe { IsIconic(hwnd) } != 0 {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
    }

    pub fn show(window: &Window) {
        if let Some(hwnd) = hwnd(window) {
            unsafe { ShowWindow(hwnd, SW_RESTORE) };
        }
    }

    fn hwnd(window: &Window) -> Option<HWND> {
        let handle = raw_window_handle::HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return None;
        };
        Some(handle.hwnd.get() as HWND)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use anyhow::Result;
    use gpui_kit::Window;

    pub struct MinimizeHandler;

    pub fn install_minimize_handler(_window: &Window) -> Result<MinimizeHandler> {
        Ok(MinimizeHandler)
    }

    pub fn show(_window: &Window) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_menu_commands_are_stable() {
        assert_eq!(
            command_for_menu_id(SHOW_MAIN_MENU_ID),
            Some(TrayCommand::ShowMainMenu)
        );
        assert_eq!(command_for_menu_id(QUIT_ID), Some(TrayCommand::Quit));
        assert_eq!(command_for_menu_id("unknown"), None);
        let tray_id = TrayIconId::new(TRAY_ID);
        assert_eq!(
            command_for_tray_click(&tray_id, MouseButton::Left, MouseButtonState::Up),
            Some(TrayCommand::ShowMainMenu)
        );
        assert_eq!(
            command_for_tray_click(&tray_id, MouseButton::Left, MouseButtonState::Down),
            None
        );
        assert_eq!(
            command_for_tray_click(&tray_id, MouseButton::Right, MouseButtonState::Up),
            None
        );
        assert_eq!(
            command_for_tray_click(
                &TrayIconId::new("another-tray"),
                MouseButton::Left,
                MouseButtonState::Up,
            ),
            None
        );
        assert!(load_icon().is_ok());
    }
}
