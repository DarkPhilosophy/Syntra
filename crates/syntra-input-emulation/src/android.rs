//! Emulation on Android through Syntra's accessibility service.
//!
//! Apps cannot inject input on Android. The app ships an accessibility
//! service (`io.syntra.syntra.SyntraAccessibilityService`) that draws a
//! pointer, turns clicks into taps and the wheel into swipes, and types into
//! the focused field. This backend forwards events to its static entry
//! points over JNI. It only exists while the user has enabled the service,
//! so the phone never invites a computer's pointer it cannot move.

use async_trait::async_trait;
use jni::{
    JNIEnv, JavaVM,
    objects::{JClass, JObject, JValue},
};
use syntra_input_event::{BTN_LEFT, Event, KeyboardEvent, PointerEvent};

use super::{Emulation, EmulationHandle, error::EmulationError};

const SERVICE_CLASS: &str = "io.syntra.syntra.SyntraAccessibilityService";
const KEY_BACKSPACE: u32 = 14;
const KEY_ENTER: u32 = 28;
const KEY_ESC: u32 = 1;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;

/// Why the accessibility backend is not available.
#[derive(Debug, thiserror::Error)]
pub enum AndroidEmulationCreationError {
    #[error("the Syntra accessibility service is not enabled")]
    ServiceDisabled,
    #[error("JNI: {0}")]
    Jni(#[from] jni::errors::Error),
}

/// Calls `f` with the service class, loaded through the app's class loader
/// (a native thread's default loader only sees system classes).
fn with_service<T>(
    f: impl FnOnce(&mut JNIEnv<'_>, &JClass<'_>) -> jni::errors::Result<T>,
) -> jni::errors::Result<T> {
    let context = ndk_context::android_context();
    // SAFETY: android-activity initialises ndk-context before android_main;
    // the VM and the activity stay owned by the runtime for the process.
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) }?;
    let mut env = vm.attach_current_thread()?;
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
        let name = env.new_string(SERVICE_CLASS)?;
        let class = env
            .call_method(
                &loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[JValue::Object(&name)],
            )?
            .l()?;
        let class = JClass::from(class);
        f(&mut env, &class)
    })();
    // The global reference belongs to the runtime; never delete it.
    std::mem::forget(activity);
    result
}

/// Whether the user has enabled the accessibility service.
pub fn service_enabled() -> bool {
    with_service(|env, class| env.call_static_method(class, "isEnabled", "()Z", &[])?.z())
        .unwrap_or(false)
}

fn call(method: &str, signature: &str, args: &[JValue<'_, '_>]) {
    if let Err(error) = with_service(|env, class| {
        env.call_static_method(class, method, signature, args)?;
        Ok(())
    }) {
        log::warn!("accessibility {method}: {error}");
    }
}

pub(crate) struct AndroidEmulation {
    shift: bool,
}

impl AndroidEmulation {
    pub(crate) fn new() -> Result<Self, AndroidEmulationCreationError> {
        if !service_enabled() {
            return Err(AndroidEmulationCreationError::ServiceDisabled);
        }
        Ok(Self { shift: false })
    }
}

#[async_trait]
impl Emulation for AndroidEmulation {
    fn healthy(&self) -> bool {
        service_enabled()
    }

    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => {
                call(
                    "motion",
                    "(FF)V",
                    &[JValue::Float(dx as f32), JValue::Float(dy as f32)],
                );
            }
            Event::Pointer(PointerEvent::Button { button, state, .. }) if button == BTN_LEFT => {
                call("button", "(Z)V", &[JValue::Bool(u8::from(state != 0))]);
            }
            // Secondary click has no touch equivalent; treat it as Back,
            // which is what a right click means to most people on a phone.
            Event::Pointer(PointerEvent::Button { state, .. }) if state != 0 => {
                call("back", "()V", &[]);
            }
            Event::Pointer(PointerEvent::AxisDiscrete120 { axis: 0, value }) => {
                call("scroll", "(F)V", &[JValue::Float(-(value as f32) / 120.0)]);
            }
            Event::Pointer(PointerEvent::Axis { axis: 0, value, .. }) => {
                call("scroll", "(F)V", &[JValue::Float(-(value as f32) / 15.0)]);
            }
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                if key == KEY_LEFTSHIFT || key == KEY_RIGHTSHIFT {
                    self.shift = state != 0;
                } else if state != 0 {
                    match key {
                        KEY_BACKSPACE => call("backspace", "()V", &[]),
                        KEY_ESC => call("back", "()V", &[]),
                        KEY_ENTER => {}
                        _ => {
                            if let Some(ch) = us_char(key, self.shift) {
                                // The Java string must live in the same JNI
                                // frame as the call that uses it.
                                if let Err(error) = with_service(|env, class| {
                                    let text = env.new_string(ch.to_string())?;
                                    env.call_static_method(
                                        class,
                                        "text",
                                        "(Ljava/lang/String;)V",
                                        &[JValue::Object(&text)],
                                    )?;
                                    Ok(())
                                }) {
                                    log::warn!("accessibility text: {error}");
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}
    async fn destroy(&mut self, _handle: EmulationHandle) {
        call("leave", "()V", &[]);
    }
    async fn terminate(&mut self) {
        call("leave", "()V", &[]);
    }
}

/// Character typed by a Linux key code on a US layout.
fn us_char(key: u32, shift: bool) -> Option<char> {
    const ROWS: [(u32, &str, &str); 4] = [
        (2, "1234567890-=", "!@#$%^&*()_+"),
        (16, "qwertyuiop[]", "QWERTYUIOP{}"),
        (30, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
        (44, "zxcvbnm,./", "ZXCVBNM<>?"),
    ];
    if key == 57 {
        return Some(' ');
    }
    if key == 43 {
        return Some(if shift { '|' } else { '\\' });
    }
    ROWS.iter().find_map(|(start, plain, shifted)| {
        let index = key.checked_sub(*start)? as usize;
        let row = if shift { shifted } else { plain };
        row.chars().nth(index)
    })
}

#[cfg(test)]
mod tests {
    use super::us_char;

    #[test]
    fn maps_us_layout_rows() {
        assert_eq!(us_char(30, false), Some('a'));
        assert_eq!(us_char(30, true), Some('A'));
        assert_eq!(us_char(2, true), Some('!'));
        assert_eq!(us_char(53, false), Some('/'));
        assert_eq!(us_char(57, false), Some(' '));
        assert_eq!(us_char(200, false), None);
    }
}
