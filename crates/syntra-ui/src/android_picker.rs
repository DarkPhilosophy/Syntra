//! System document picker for the Android app.
//!
//! The picker runs in `io.syntra.syntra.PickerActivity`, which copies the
//! chosen documents into the app cache; this module starts it and waits for
//! the paths without blocking the UI thread.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use jni::JavaVM;
use jni::objects::{JObject, JString, JValue};

const PICKER_CLASS: &str = "io.syntra.syntra.PickerActivity";

fn with_picker<T>(
    f: impl FnOnce(
        &mut jni::JNIEnv<'_>,
        &jni::objects::JClass<'_>,
        &JObject<'_>,
    ) -> jni::errors::Result<T>,
) -> io::Result<T> {
    let context = ndk_context::android_context();
    // SAFETY: ndk-context is initialised before android_main; the VM and the
    // activity outlive the process and are only borrowed here.
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) }.map_err(io::Error::other)?;
    let mut env = vm.attach_current_thread().map_err(io::Error::other)?;
    // SAFETY: ndk-context holds the activity as a JNI global reference.
    let activity = unsafe { JObject::from_raw(context.context().cast()) };
    let result = (|| {
        let loader = env
            .call_method(
                &activity,
                "getClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[],
            )?
            .l()?;
        let name = env.new_string(PICKER_CLASS)?;
        let class = env
            .call_method(
                &loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[JValue::Object(&name)],
            )?
            .l()?;
        let class = jni::objects::JClass::from(class);
        f(&mut env, &class, &activity)
    })();
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    std::mem::forget(activity);
    result.map_err(io::Error::other)
}

/// Opens the picker and returns the chosen files, empty when cancelled.
pub async fn pick(mime: &str, multiple: bool) -> io::Result<Vec<PathBuf>> {
    with_picker(|env, class, activity| {
        let mime = env.new_string(mime)?;
        env.call_static_method(
            class,
            "launch",
            "(Landroid/content/Context;Ljava/lang/String;Z)V",
            &[
                JValue::Object(activity),
                JValue::Object(&mime),
                JValue::Bool(u8::from(multiple)),
            ],
        )?;
        Ok(())
    })?;
    loop {
        let result = with_picker(|env, class, _| {
            let value = env
                .call_static_method(class, "takeResult", "()Ljava/lang/String;", &[])?
                .l()?;
            if value.is_null() {
                return Ok(None);
            }
            let text: String = env.get_string(&JString::from(value))?.into();
            Ok(Some(text))
        })?;
        if let Some(text) = result {
            return Ok(text
                .lines()
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect());
        }
        // Waits on the UI loop's timer, so the interface stays responsive.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        slint::Timer::single_shot(Duration::from_millis(200), move || {
            let _ = tx.send(());
        });
        let _ = rx.await;
    }
}

/// Moves a received file into the shared Downloads/Syntra folder. Returns
/// where the user finds it, or `None` when it stays in the app folder.
///
/// The file is moved, so a completed transfer reported again (every UI
/// reconnect replays them) finds nothing to publish and is left alone.
pub fn publish_download(path: &std::path::Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let path = path.to_string_lossy().into_owned();
    with_picker(|env, class, activity| {
        let path = env.new_string(path)?;
        let visible = env
            .call_static_method(
                class,
                "publishDownload",
                "(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(activity), JValue::Object(&path)],
            )?
            .l()?;
        if visible.is_null() {
            return Ok(None);
        }
        Ok(Some(env.get_string(&JString::from(visible))?.into()))
    })
    .ok()
    .flatten()
}
