//! macscp —— 原生 macOS SFTP 客户端。
//!
//! 线程模型：
//! * 主线程 = GPUI（渲染、输入、实体状态）；
//! * `macscp-sftp` 线程 = 独立 Tokio runtime（SSH/SFTP I/O）；
//! * 两者只通过 `flume` channel 通信，见 `sftp::worker`。

mod app;
#[cfg(feature = "demo-server")]
mod demo;
mod sftp;
mod session;
mod ui;

use gpui_kit::*;

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,macscp=debug"),
    )
    .init();

    // 注册图标资源：AllAssets 把完整 Lucide 图标集嵌进二进制，
    // 否则所有 IconName 都会渲染成空白。
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx| {
        gpui_kit::init(cx);

        // 裸二进制没有 bundle，Dock 图标得运行时自己设（.app 启动时是重复设置，无害）。
        ui::app_icon::install();

        // 演示模式：先起内置服务器，等它把端口报回来再开窗。
        #[cfg(feature = "demo-server")]
        let _ = demo::start();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1200.), px(780.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("macscp".into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            ..Default::default()
        };

        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| {
                #[cfg_attr(not(feature = "demo-server"), allow(unused_mut))]
                let mut app = app::ScpApp::new(window, cx);
                #[cfg(feature = "demo-server")]
                app.apply_demo_login(window, cx);
                app
            })
        })
        .expect("failed to open the main window");

        cx.activate(true);
    });
}
