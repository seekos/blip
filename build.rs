fn main() {
    println!("cargo:rerun-if-changed=assets/icon/AppIcon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/icon/AppIcon.ico")
            .compile()
            .expect("failed to embed the Windows icon");
    }
}
