use std::{env, fs, path::PathBuf};

fn main() {
    let shared = PathBuf::from("../src/session/store.rs");
    println!("cargo:rerun-if-changed={}", shared.display());

    let mut source = fs::read_to_string(&shared).expect("read shared session store");
    source = source.replacen("//!", "//", 1);
    source.push_str(
        r#"

// A mobile sandbox home is installation-specific and is also mixed into the
// bound unlock key by the shared store, so no desktop machine ID is needed.
#[cfg(any(target_os = "ios", target_os = "android"))]
fn machine_fingerprint() -> Result<String, String> {
    Ok("blip-mobile-sandbox-v1".to_string())
}
"#,
    );

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("session_store.rs");
    fs::write(output, source).expect("write mobile session store adapter");
}
