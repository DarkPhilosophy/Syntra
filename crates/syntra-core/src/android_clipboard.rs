//! Android text clipboard adapter for the shared clipboard observer.
use jni::{
    JNIEnv, JavaVM,
    objects::{JObject, JString, JValue},
};
use std::{borrow::Cow, fmt};

#[derive(Debug)]
pub enum Error {
    ContentNotAvailable,
    ConversionFailure,
    Java(jni::errors::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContentNotAvailable => write!(f, "clipboard content unavailable"),
            Self::ConversionFailure => write!(f, "clipboard conversion failed"),
            Self::Java(error) => write!(f, "Android clipboard JNI: {error}"),
        }
    }
}
impl From<jni::errors::Error> for Error {
    fn from(error: jni::errors::Error) -> Self {
        Self::Java(error)
    }
}

pub struct ImageData<'a> {
    pub width: usize,
    pub height: usize,
    pub bytes: Cow<'a, [u8]>,
}
impl ImageData<'_> {
    pub fn into_owned_bytes(self) -> Cow<'static, [u8]> {
        Cow::Owned(self.bytes.into_owned())
    }
}

pub struct Clipboard;
impl Clipboard {
    pub fn new() -> Result<Self, Error> {
        Ok(Self)
    }
    fn with_manager<T>(
        f: impl FnOnce(&mut JNIEnv<'_>, &JObject<'_>, &JObject<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let context = ndk_context::android_context();
        // SAFETY: android-activity initializes ndk-context before android_main;
        // the VM and activity remain owned by the Android runtime for this process.
        let vm = unsafe { JavaVM::from_raw(context.vm().cast()) }?;
        let mut env = vm.attach_current_thread()?;
        // SAFETY: ndk-context holds the activity as a JNI global reference.
        let activity = unsafe { JObject::from_raw(context.context().cast()) };
        let result = (|| {
            let name = env.new_string("clipboard")?;
            let manager = env
                .call_method(
                    &activity,
                    "getSystemService",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[JValue::Object(&name)],
                )?
                .l()?;
            if manager.is_null() {
                return Err(Error::ContentNotAvailable);
            }
            f(&mut env, &activity, &manager)
        })();
        // JObject::from_raw must not delete the runtime-owned global reference.
        std::mem::forget(activity);
        result
    }
    pub fn get(&mut self) -> &mut Self {
        self
    }
    pub fn file_list(&mut self) -> Result<Vec<String>, Error> {
        Err(Error::ContentNotAvailable)
    }
    pub fn get_text(&mut self) -> Result<String, Error> {
        Self::with_manager(|env, _activity, manager| {
            let clip = env
                .call_method(
                    manager,
                    "getPrimaryClip",
                    "()Landroid/content/ClipData;",
                    &[],
                )?
                .l()?;
            if clip.is_null() {
                return Err(Error::ContentNotAvailable);
            }
            let count = env.call_method(&clip, "getItemCount", "()I", &[])?.i()?;
            if count == 0 {
                return Err(Error::ContentNotAvailable);
            }
            let item = env
                .call_method(
                    &clip,
                    "getItemAt",
                    "(I)Landroid/content/ClipData$Item;",
                    &[JValue::Int(0)],
                )?
                .l()?;
            let text = env
                .call_method(item, "getText", "()Ljava/lang/CharSequence;", &[])?
                .l()?;
            if text.is_null() {
                return Err(Error::ContentNotAvailable);
            }
            let value = env
                .call_method(text, "toString", "()Ljava/lang/String;", &[])?
                .l()?;
            Ok(env.get_string(&JString::from(value))?.into())
        })
    }
    pub fn get_image(&mut self) -> Result<ImageData<'static>, Error> {
        Err(Error::ContentNotAvailable)
    }
    pub fn set_text(&mut self, text: String) -> Result<(), Error> {
        Self::with_manager(|env, _, manager| {
            let label = env.new_string("Syntra")?;
            let text = env.new_string(text)?;
            let clip = env
                .call_static_method(
                    "android/content/ClipData",
                    "newPlainText",
                    "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                    &[JValue::Object(&label), JValue::Object(&text)],
                )?
                .l()?;
            env.call_method(
                manager,
                "setPrimaryClip",
                "(Landroid/content/ClipData;)V",
                &[JValue::Object(&clip)],
            )?;
            Ok(())
        })
    }
    pub fn set_image(&mut self, _image: ImageData<'_>) -> Result<(), Error> {
        Err(Error::ContentNotAvailable)
    }
}
