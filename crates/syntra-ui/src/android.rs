//! Android application host: an in-process service and desktop-compatible IPC dashboard.

#[cfg(target_os = "android")]
struct MulticastLock(jni::objects::GlobalRef);

#[cfg(target_os = "android")]
impl MulticastLock {
    fn acquire() -> Result<Self, Box<dyn std::error::Error>> {
        use jni::{
            JavaVM,
            objects::{JObject, JValue},
        };
        let context = ndk_context::android_context();
        // SAFETY: android-activity initializes this process-global VM and
        // application global ref before entering android_main.
        let vm = unsafe { JavaVM::from_raw(context.vm().cast()) }?;
        let mut env = vm.attach_current_thread()?;
        let activity = unsafe { JObject::from_raw(context.context().cast()) };
        let result = (|| {
            let wifi_name = env.new_string("wifi")?;
            let wifi = env
                .call_method(
                    &activity,
                    "getSystemService",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[JValue::Object(&wifi_name)],
                )?
                .l()?;
            if wifi.is_null() {
                return Err("WifiManager unavailable".into());
            }
            let tag = env.new_string("Syntra discovery")?;
            let lock = env
                .call_method(
                    wifi,
                    "createMulticastLock",
                    "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;",
                    &[JValue::Object(&tag)],
                )?
                .l()?;
            env.call_method(&lock, "setReferenceCounted", "(Z)V", &[JValue::Bool(0)])?;
            let owned = env.new_global_ref(&lock)?;
            env.call_method(&lock, "acquire", "()V", &[])?;
            Ok(Self(owned))
        })();
        std::mem::forget(activity);
        result
    }
}

#[cfg(target_os = "android")]
impl Drop for MulticastLock {
    fn drop(&mut self) {
        use jni::JavaVM;
        let context = ndk_context::android_context();
        if let Ok(vm) = unsafe { JavaVM::from_raw(context.vm().cast()) } {
            if let Ok(mut env) = vm.attach_current_thread() {
                if let Err(error) = env.call_method(&self.0, "release", "()V", &[]) {
                    log::warn!("Could not release mDNS multicast lock: {error}");
                }
            }
        }
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub fn android_main(app: slint::android::AndroidApp) {
    // Without this bridge every Rust log line is discarded, which makes a
    // failing start look like the app simply doing nothing.
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("syntra"),
    );
    log::info!("Syntra Android entry reached");
    // Emulators and Waydroid expose a GL stack Skia cannot build a direct
    // context on, which aborts start-up. Honour an explicit choice, and
    // otherwise ask for the software renderer, which always works.
    if std::env::var_os("SLINT_RENDERER").is_none() {
        // SAFETY: single-threaded, before any Slint initialisation.
        unsafe { std::env::set_var("SLINT_RENDERER", "software") };
    }
    let Some(data_path) = app.internal_data_path() else {
        log::error!("Android did not provide an internal data directory");
        return;
    };
    let config_dir = data_path.join("syntra");
    if let Err(error) = std::fs::create_dir_all(&config_dir) {
        log::error!("Could not create Android service directory: {error}");
        return;
    }
    // Set once before starting either IPC client or service worker.
    unsafe {
        std::env::set_var(syntra_api::paths::ENV_CONFIG_DIR, &config_dir);
        std::env::set_var(
            syntra_api::paths::ENV_DAEMON_SOCKET,
            data_path.join("syntra-daemon.sock"),
        );
        std::env::set_var(
            syntra_api::paths::ENV_DIAGNOSTICS_SOCKET,
            data_path.join("syntra-diagnostics.sock"),
        );
    }
    let multicast_lock = match MulticastLock::acquire() {
        Ok(lock) => Some(lock),
        Err(error) => {
            log::warn!("mDNS multicast lock unavailable: {error}");
            None
        }
    };
    let worker = std::thread::Builder::new()
        .name("syntra-android-service".into())
        .spawn(move || {
            let _multicast_lock = multicast_lock;
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    log::error!("Android service runtime failed: {error}");
                    return;
                }
            };
            let log_config = syntra_log::LogConfig::from_env(syntra_api::paths::ENV_LOG, "info");
            let result = runtime.block_on(tokio::task::LocalSet::new().run_until(async move {
                let config = syntra_core::config::Config::new()?;
                let mut service = syntra_core::service::Service::new(config, log_config).await?;
                service.run().await?;
                Ok::<(), Box<dyn std::error::Error>>(())
            }));
            if let Err(error) = result {
                log::error!("Android service stopped: {error}");
            }
        });
    if let Err(error) = worker {
        log::error!("Could not spawn Android service: {error}");
    }
    spawn_accessibility_watcher();
    slint::android::init(app).expect("failed to initialize Slint Android backend");
    if let Err(error) = crate::app::run_with_startup(crate::app::StartupMode::Window) {
        log::error!("Slint Android UI failed: {error}");
    }
}

/// Re-enables emulation once the user turns the accessibility service on,
/// so the phone becomes controllable without restarting the app.
fn spawn_accessibility_watcher() {
    let _ = std::thread::Builder::new()
        .name("syntra-accessibility-watch".into())
        .spawn(|| {
            let mut enabled = false;
            loop {
                let now = syntra_input_emulation::android::service_enabled();
                if now && !enabled {
                    log::info!("accessibility service enabled; starting emulation");
                    request(r#""EnableEmulation""#);
                }
                enabled = now;
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        });
}

/// Sends one request to the in-process service over its control socket.
fn request(json: &str) {
    use std::io::Write;
    let Ok(path) = syntra_api::default_socket_path() else {
        return;
    };
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(mut stream) => {
            let _ = stream.write_all(format!("{json}\n").as_bytes());
        }
        Err(error) => log::warn!("service request failed: {error}"),
    }
}

/// Opens the system Accessibility settings, where the user enables
/// "Syntra remote control".
pub fn open_accessibility_settings() {
    use jni::{
        JavaVM,
        objects::{JObject, JValue},
    };
    let context = ndk_context::android_context();
    let result = (|| -> jni::errors::Result<()> {
        // SAFETY: ndk-context is initialised before android_main and the VM
        // and activity outlive the process.
        let vm = unsafe { JavaVM::from_raw(context.vm().cast()) }?;
        let mut env = vm.attach_current_thread()?;
        let activity = unsafe { JObject::from_raw(context.context().cast()) };
        let outcome = (|| {
            let action = env.new_string("android.settings.ACCESSIBILITY_SETTINGS")?;
            let intent = env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&action)],
            )?;
            // The context ndk-context hands out is the application, not an
            // activity; starting an activity from it requires a new task.
            const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
            env.call_method(
                &intent,
                "addFlags",
                "(I)Landroid/content/Intent;",
                &[JValue::Int(FLAG_ACTIVITY_NEW_TASK)],
            )?;
            env.call_method(
                &activity,
                "startActivity",
                "(Landroid/content/Intent;)V",
                &[JValue::Object(&intent)],
            )?;
            Ok(())
        })();
        // A Java exception left pending aborts the process on the next JNI
        // call; report it and clear it instead.
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        std::mem::forget(activity);
        outcome
    })();
    if let Err(error) = result {
        log::warn!("could not open accessibility settings: {error}");
    }
}

/// Whether the user has enabled phone control.
pub fn phone_control_enabled() -> bool {
    syntra_input_emulation::android::service_enabled()
}
