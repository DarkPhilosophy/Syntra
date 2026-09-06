#![allow(clashing_extern_declarations)]

use std::ffi::{CStr, CString, c_char, c_double, c_uchar, c_uint, c_void};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use super::{
    CloseBehavior, PlatformAction, PlatformActions, PlatformCallbacks, PlatformCapabilities,
    PlatformError, PlatformTray,
};

type Id = *mut c_void;
type Class = *mut c_void;
type Sel = *mut c_void;
type ObjcBool = i8;
static STATUS_ITEM: Mutex<Option<usize>> = Mutex::new(None);

pub struct MacPlatform;

impl MacPlatform {
    pub fn new() -> Self {
        Self
    }
}

impl PlatformActions for MacPlatform {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            tray: true,
            close_behavior: CloseBehavior::Hide,
            flatpak_permissions: false,
        }
    }

    fn perform(&self, action: PlatformAction) -> Result<(), PlatformError> {
        match action {
            PlatformAction::RequestPermissions => {
                fire_initial_prompts();
                Ok(())
            }
            PlatformAction::OpenPermissionSettings => Command::new("open")
                .arg(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
                )
                .spawn()
                .map(|_| ())
                .map_err(|e| PlatformError::Permission(e.to_string())),
            PlatformAction::RelaunchAfterPermissionGrant => relaunch_bundle(),
            PlatformAction::Show => {
                callbacks()
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(PlatformCallbacks::show);
                Ok(())
            }
            PlatformAction::Quit => {
                callbacks()
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(PlatformCallbacks::quit);
                Ok(())
            }
            PlatformAction::GrantFlatpakFilesystemAccess { .. } => Err(
                PlatformError::UnsupportedAction("Flatpak permissions are Linux-only"),
            ),
            PlatformAction::NotifyRunningInBackground => Ok(()),
        }
    }

    fn start_tray(
        &self,
        platform_callbacks: PlatformCallbacks,
    ) -> Result<Box<dyn PlatformTray>, PlatformError> {
        *callbacks()
            .lock()
            .map_err(|e| PlatformError::Tray(e.to_string()))? = Some(platform_callbacks.clone());
        unsafe {
            setup_status_item()?;
        }
        fire_initial_prompts();

        let running = Arc::new(AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let worker = std::thread::Builder::new()
            .name("lan-mouse-accessibility".into())
            .spawn(move || {
                let mut last = accessibility_granted();
                while worker_running.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_secs(1));
                    let current = accessibility_granted();
                    if current != last {
                        if current {
                            platform_callbacks.show();
                        } else {
                            platform_callbacks.quit();
                        }
                        last = current;
                    }
                }
            })
            .map_err(|e| {
                if let Ok(mut callbacks) = callbacks().lock() {
                    *callbacks = None;
                }
                PlatformError::Permission(e.to_string())
            })?;
        Ok(Box::new(MacOsTray {
            running,
            worker: Some(worker),
        }))
    }
}

struct MacOsTray {
    running: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl PlatformTray for MacOsTray {}
impl Drop for MacOsTray {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        unsafe {
            remove_status_item();
        }
        *callbacks().lock().unwrap() = None;
    }
}
unsafe fn remove_status_item() {
    let status_bar = msg_send_id(class(c"NSStatusBar"), sel(c"systemStatusBar"));
    if !status_bar.is_null() {
        if let Some(item) = STATUS_ITEM.lock().unwrap().take() {
            msg_send_void_id(status_bar, sel(c"removeStatusItem:"), item as Id);
            msg_send_void_id(item as Id, sel(c"release"), std::ptr::null_mut());
        }
    }
}

fn callbacks() -> &'static Mutex<Option<PlatformCallbacks>> {
    static CALLBACKS: OnceLock<Mutex<Option<PlatformCallbacks>>> = OnceLock::new();
    CALLBACKS.get_or_init(|| Mutex::new(None))
}

fn accessibility_granted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

fn fire_initial_prompts() {
    static FIRED: Once = Once::new();
    FIRED.call_once(|| unsafe {
        if !accessibility_granted() {
            let key = kAXTrustedCheckOptionPrompt;
            let value = kCFBooleanTrue;
            let options = CFDictionaryCreate(
                kCFAllocatorDefault,
                &key,
                &value,
                1,
                kCFTypeDictionaryKeyCallBacks,
                kCFTypeDictionaryValueCallBacks,
            );
            if !options.is_null() {
                AXIsProcessTrustedWithOptions(options);
                CFRelease(options);
            }
            return;
        }
        ensure_listed_in_input_monitoring();
        CGRequestPostEventAccess();
    });
}

unsafe fn ensure_listed_in_input_monitoring() {
    CGRequestListenEventAccess();
    let tap = CGEventTapCreate(
        1,
        0,
        1,
        1 << 10,
        input_monitoring_noop_tap_callback as *const c_void,
        std::ptr::null(),
    );
    if !tap.is_null() {
        CFRelease(tap);
    }
}

extern "C" fn input_monitoring_noop_tap_callback(
    _: *const c_void,
    _: u32,
    event: *const c_void,
    _: *const c_void,
) -> *const c_void {
    event
}

fn relaunch_bundle() -> Result<(), PlatformError> {
    let exe = std::env::current_exe().map_err(|e| PlatformError::Permission(e.to_string()))?;
    let bundle = exe
        .parent()
        .and_then(std::path::Path::parent)
        .and_then(std::path::Path::parent)
        .ok_or_else(|| {
            PlatformError::Permission("executable is not inside a macOS application bundle".into())
        })?;
    let cmd = format!("(sleep 1 && open {bundle:?}) &");
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .spawn()
        .map(|_| ())
        .map_err(|e| PlatformError::Permission(e.to_string()))
}

unsafe fn setup_status_item() -> Result<(), PlatformError> {
    if STATUS_ITEM.lock().unwrap().is_some() {
        return Ok(());
    }
    let ns_app = msg_send_id(class(c"NSApplication"), sel(c"sharedApplication"));
    if ns_app.is_null() {
        return Err(PlatformError::Tray("NSApplication unavailable".into()));
    }
    msg_send_bool_usize(ns_app, sel(c"setActivationPolicy:"), 1);
    let delegate = new_delegate()?;
    let menu = msg_send_id(msg_send_id(class(c"NSMenu"), sel(c"alloc")), sel(c"init"));
    let open = menu_item(c"Open Syntra", c"showLanMouse:");
    let separator = msg_send_id(class(c"NSMenuItem"), sel(c"separatorItem"));
    let quit = menu_item(c"Quit Syntra", c"quitLanMouse:");
    for item in [open, separator, quit] {
        msg_send_void_id(menu, sel(c"addItem:"), item);
        msg_send_void_id(item, sel(c"setTarget:"), delegate);
    }
    let status_bar = msg_send_id(class(c"NSStatusBar"), sel(c"systemStatusBar"));
    let status_item = msg_send_id_f64(status_bar, sel(c"statusItemWithLength:"), -1.0);
    if status_item.is_null() {
        return Err(PlatformError::Tray("NSStatusItem creation failed".into()));
    }
    let status_item = msg_send_id(status_item, sel(c"retain"));
    let button = msg_send_id(status_item, sel(c"button"));
    msg_send_void_id(button, sel(c"setTitle:"), nsstring(c"Syntra"));
    msg_send_void_id(button, sel(c"setToolTip:"), nsstring(c"Syntra"));
    msg_send_void_id(status_item, sel(c"setMenu:"), menu);
    install_reopen_handler(delegate);
    let _ = STATUS_ITEM.lock().unwrap().replace(status_item as usize);
    Ok(())
}

unsafe fn new_delegate() -> Result<Id, PlatformError> {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let class = *CLASS.get_or_init(|| {
        let name = CString::new("LanMouseSlintStatusItemDelegate").unwrap();
        let cls = objc_allocateClassPair(class(c"NSObject"), name.as_ptr(), 0);
        if cls.is_null() {
            return 0;
        }
        class_addMethod(
            cls,
            sel(c"showLanMouse:"),
            show_lan_mouse as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            cls,
            sel(c"quitLanMouse:"),
            quit_lan_mouse as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            cls,
            sel(c"handleReopenEvent:withReplyEvent:"),
            handle_reopen as *const c_void,
            c"v@:@@".as_ptr(),
        );
        objc_registerClassPair(cls);
        cls as usize
    });
    if class == 0 {
        return Err(PlatformError::Tray(
            "Objective-C delegate registration failed".into(),
        ));
    }
    Ok(msg_send_id(
        msg_send_id(class as Class, sel(c"alloc")),
        sel(c"init"),
    ))
}

unsafe fn menu_item(title: &CStr, action: &CStr) -> Id {
    msg_send_id_id_sel_id(
        msg_send_id(class(c"NSMenuItem"), sel(c"alloc")),
        sel(c"initWithTitle:action:keyEquivalent:"),
        nsstring(title),
        sel(action),
        nsstring(c""),
    )
}

extern "C" fn show_lan_mouse(_: Id, _: Sel, _: Id) {
    if let Ok(guard) = callbacks().lock() {
        if let Some(cb) = guard.as_ref() {
            cb.show();
        }
    }
}
extern "C" fn quit_lan_mouse(_: Id, _: Sel, _: Id) {
    if let Ok(guard) = callbacks().lock() {
        if let Some(cb) = guard.as_ref() {
            cb.quit();
        }
    }
}
extern "C" fn handle_reopen(_: Id, _: Sel, _: Id, _: Id) {
    show_lan_mouse(
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
}

unsafe fn install_reopen_handler(delegate: Id) {
    let manager = msg_send_id(
        class(c"NSAppleEventManager"),
        sel(c"sharedAppleEventManager"),
    );
    if !manager.is_null() {
        msg_send_void_id_sel_u32_u32(
            manager,
            sel(c"setEventHandler:andSelector:forEventClass:andEventID:"),
            delegate,
            sel(c"handleReopenEvent:withReplyEvent:"),
            0x6165_7674,
            0x7261_7070,
        );
    }
}
unsafe fn class(name: &CStr) -> Class {
    objc_getClass(name.as_ptr())
}
unsafe fn sel(name: &CStr) -> Sel {
    sel_registerName(name.as_ptr())
}
unsafe fn nsstring(value: &CStr) -> Id {
    msg_send_id_ptr(
        class(c"NSString"),
        sel(c"stringWithUTF8String:"),
        value.as_ptr(),
    )
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> c_uchar;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> c_uchar;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFAllocatorDefault: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: *const c_void;
    static kCFTypeDictionaryValueCallBacks: *const c_void;
    static kCFBooleanTrue: *const c_void;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGRequestListenEventAccess() -> c_uchar;
    fn CGRequestPostEventAccess() -> c_uchar;
    fn CGEventTapCreate(
        tap: u32,
        placement: u32,
        options: u32,
        mask: u64,
        callback: *const c_void,
        user_info: *const c_void,
    ) -> *const c_void;
}
#[link(name = "AppKit", kind = "framework")]
extern "C" {}
#[link(name = "objc")]
extern "C" {
    fn objc_allocateClassPair(superclass: Class, name: *const c_char, extra: usize) -> Class;
    fn objc_getClass(name: *const c_char) -> Class;
    fn objc_registerClassPair(class: Class);
    fn sel_registerName(name: *const c_char) -> Sel;
    fn class_addMethod(
        class: Class,
        name: Sel,
        imp: *const c_void,
        types: *const c_char,
    ) -> ObjcBool;
    #[link_name = "objc_msgSend"]
    fn msg_send_id(receiver: Id, selector: Sel) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_f64(receiver: Id, selector: Sel, value: c_double) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_id_sel_id(receiver: Id, selector: Sel, a: Id, b: Sel, c: Id) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_ptr(receiver: Id, selector: Sel, value: *const c_char) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_void_id(receiver: Id, selector: Sel, value: Id);
    #[link_name = "objc_msgSend"]
    fn msg_send_bool_usize(receiver: Id, selector: Sel, value: usize) -> ObjcBool;
    #[link_name = "objc_msgSend"]
    fn msg_send_void_id_sel_u32_u32(
        receiver: Id,
        selector: Sel,
        a: Id,
        b: Sel,
        c: c_uint,
        d: c_uint,
    );
}
