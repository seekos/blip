//! Android Storage Access Framework adapter for importing a selected tree.

use jni::objects::{JObject, JValue};

pub fn pick_directory() -> Result<Option<String>, String> {
    pick_directory_with_mode(false)
}

pub fn pick_shared_directory() -> Result<Option<String>, String> {
    pick_directory_with_mode(true)
}

fn pick_directory_with_mode(shared: bool) -> Result<Option<String>, String> {
    gpui_mobile::android::jni::with_env(|env| {
        let activity = gpui_mobile::android::jni::activity(env)?;
        let class = gpui_mobile::android::jni::find_app_class(
            env,
            "com.amov.blip.mobile.BlipDirectoryPicker",
        )?;
        let result = if shared {
            env.call_static_method(
                &class,
                jni::jni_str!("openSharedDirectory"),
                jni::jni_sig!("(Landroid/app/Activity;)Ljava/lang/String;"),
                &[JValue::Object(&activity)],
            )
        } else {
            env.call_static_method(
                &class,
                jni::jni_str!("openDirectory"),
                jni::jni_sig!("(Landroid/app/Activity;)Ljava/lang/String;"),
                &[JValue::Object(&activity)],
            )
        }
        .and_then(|value| value.l())
        .map_err(|error| {
            env.exception_clear();
            error.to_string()
        })?;
        if result.is_null() {
            return Ok(None);
        }
        let path = gpui_mobile::android::jni::get_string(env, &JObject::from(result));
        Ok(Some(path))
    })
}
