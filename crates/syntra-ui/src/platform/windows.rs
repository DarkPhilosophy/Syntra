use std::mem::size_of;
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, HMENU, IDI_APPLICATION, LoadIconW,
    MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
};
use windows::core::{Error as WindowsError, w};

use super::{
    CloseBehavior, PlatformAction, PlatformActions, PlatformCallbacks, PlatformCapabilities,
    PlatformError, PlatformTray,
};

const TRAY_ICON_ID: u32 = 1;
const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;
const OPEN_COMMAND: usize = 1;
const QUIT_COMMAND: usize = 2;

pub struct WindowsPlatform;

impl WindowsPlatform {
    pub fn new() -> Self {
        Self
    }
}

impl PlatformActions for WindowsPlatform {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            tray: true,
            close_behavior: CloseBehavior::Hide,
            flatpak_permissions: false,
        }
    }

    fn perform(&self, action: PlatformAction) -> Result<(), PlatformError> {
        match action {
            PlatformAction::Show => invoke_callback(PlatformCallbacks::show),
            PlatformAction::Quit => invoke_callback(PlatformCallbacks::quit),
            PlatformAction::NotifyRunningInBackground => show_background_notification(),
            PlatformAction::RequestPermissions
            | PlatformAction::OpenPermissionSettings
            | PlatformAction::RelaunchAfterPermissionGrant => Ok(()),
            PlatformAction::GrantFlatpakFilesystemAccess { .. } => Err(
                PlatformError::UnsupportedAction("Flatpak permissions are Linux-only"),
            ),
        }
    }

    fn start_tray(
        &self,
        platform_callbacks: PlatformCallbacks,
    ) -> Result<Box<dyn PlatformTray>, PlatformError> {
        let mut callback_slot = callbacks()
            .lock()
            .map_err(|error| PlatformError::Tray(error.to_string()))?;
        if callback_slot.is_some() {
            return Err(PlatformError::Tray(
                "Windows notification-area icon is already running".into(),
            ));
        }
        *callback_slot = Some(platform_callbacks);
        drop(callback_slot);

        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("lan-mouse-notification-area".into())
            .spawn(move || tray_message_loop(ready_tx))
            .map_err(|error| {
                clear_callbacks();
                PlatformError::Tray(error.to_string())
            })?;

        match ready_rx.recv() {
            Ok(Ok(raw_hwnd)) => Ok(Box::new(WindowsTray {
                hwnd: raw_hwnd,
                thread: Some(thread),
            })),
            Ok(Err(error)) => {
                let _ = thread.join();
                clear_callbacks();
                Err(PlatformError::Tray(error))
            }
            Err(error) => {
                let _ = thread.join();
                clear_callbacks();
                Err(PlatformError::Tray(error.to_string()))
            }
        }
    }
}

struct WindowsTray {
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
}

impl PlatformTray for WindowsTray {}

impl Drop for WindowsTray {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        clear_callbacks();
    }
}

fn callbacks() -> &'static Mutex<Option<PlatformCallbacks>> {
    static CALLBACKS: OnceLock<Mutex<Option<PlatformCallbacks>>> = OnceLock::new();
    CALLBACKS.get_or_init(|| Mutex::new(None))
}

fn tray_window() -> &'static Mutex<Option<isize>> {
    static TRAY_WINDOW: OnceLock<Mutex<Option<isize>>> = OnceLock::new();
    TRAY_WINDOW.get_or_init(|| Mutex::new(None))
}

fn clear_callbacks() {
    if let Ok(mut callbacks) = callbacks().lock() {
        *callbacks = None;
    }
}

fn invoke_callback(call: fn(&PlatformCallbacks)) -> Result<(), PlatformError> {
    let callbacks = callbacks()
        .lock()
        .map_err(|error| PlatformError::Tray(error.to_string()))?
        .clone();
    if let Some(callbacks) = callbacks.as_ref() {
        call(callbacks);
    }
    Ok(())
}

fn show_background_notification() -> Result<(), PlatformError> {
    let raw_hwnd = tray_window()
        .lock()
        .map_err(|error| PlatformError::Tray(error.to_string()))?
        .ok_or_else(|| {
            PlatformError::Tray("Windows notification-area icon is not running".into())
        })?;
    let hwnd = HWND(raw_hwnd as *mut _);
    let mut data = notification_data(hwnd).map_err(tray_error)?;
    data.uFlags |= NIF_INFO;
    copy_wide(&mut data.szInfoTitle, "Syntra");
    copy_wide(
        &mut data.szInfo,
        "Syntra is running in the background. Use the notification-area icon to open or quit it.",
    );
    data.dwInfoFlags = NIIF_INFO;
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data).as_bool() } {
        Ok(())
    } else {
        Err(tray_error(WindowsError::from_win32()))
    }
}

fn tray_message_loop(ready: mpsc::SyncSender<Result<isize, String>>) {
    let result = unsafe { create_tray_window() };
    let hwnd = match result {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    if let Ok(mut slot) = tray_window().lock() {
        *slot = Some(hwnd.0 as isize);
    }
    if ready.send(Ok(hwnd.0 as isize)).is_err() {
        unsafe {
            remove_tray_icon(hwnd);
            let _ = DestroyWindow(hwnd);
        }
        return;
    }

    let mut message = MSG::default();
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    if let Ok(mut slot) = tray_window().lock() {
        *slot = None;
    }
}

unsafe fn create_tray_window() -> windows::core::Result<HWND> {
    let module = GetModuleHandleW(None)?;
    let instance = HINSTANCE(module.0);
    let class_name = w!("LanMouseNotificationAreaWindow");
    let class = WNDCLASSW {
        hInstance: instance,
        lpszClassName: class_name,
        lpfnWndProc: Some(window_proc),
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        let error = WindowsError::from_win32();
        // ERROR_CLASS_ALREADY_EXISTS is harmless when a previous tray instance used this class.
        if error.code().0 != 0x8007_0582u32 as i32 {
            return Err(error);
        }
    }
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        w!("Syntra notification area"),
        WINDOW_STYLE::default(),
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        0,
        0,
        None,
        None,
        Some(instance),
        None,
    )?;
    add_tray_icon(hwnd)?;
    Ok(hwnd)
}

unsafe fn add_tray_icon(hwnd: HWND) -> windows::core::Result<()> {
    let data = notification_data(hwnd)?;
    if Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
        Ok(())
    } else {
        let error = WindowsError::from_win32();
        let _ = DestroyWindow(hwnd);
        Err(error)
    }
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    if let Ok(data) = notification_data(hwnd) {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

fn notification_data(hwnd: HWND) -> windows::core::Result<NOTIFYICONDATAW> {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_CALLBACK_MESSAGE,
        hIcon: unsafe { LoadIconW(None, IDI_APPLICATION)? },
        ..Default::default()
    };
    copy_wide(&mut data.szTip, "Syntra");
    Ok(data)
}

fn copy_wide<const N: usize>(target: &mut [u16; N], value: &str) {
    let mut encoded = value.encode_utf16();
    for slot in target.iter_mut().take(N.saturating_sub(1)) {
        let Some(unit) = encoded.next() else { break };
        *slot = unit;
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == TRAY_CALLBACK_MESSAGE {
        match lparam.0 as u32 {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                let _ = invoke_callback(PlatformCallbacks::show);
            }
            WM_RBUTTONUP => show_context_menu(hwnd),
            _ => {}
        }
        return LRESULT(0);
    }

    // Explorer broadcasts this registered message after recreating the taskbar.
    if message == RegisterWindowMessageW(w!("TaskbarCreated")) {
        let _ = add_tray_icon(hwnd);
        return LRESULT(0);
    }

    match message {
        WM_COMMAND => {
            match wparam.0 & 0xffff {
                OPEN_COMMAND => {
                    let _ = invoke_callback(PlatformCallbacks::show);
                }
                QUIT_COMMAND => {
                    let _ = invoke_callback(PlatformCallbacks::quit);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            remove_tray_icon(hwnd);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn show_context_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let menu_result = (|| -> windows::core::Result<()> {
        AppendMenuW(menu, MF_STRING, OPEN_COMMAND, w!("Open Syntra"))?;
        AppendMenuW(menu, MF_SEPARATOR, 0, None)?;
        AppendMenuW(menu, MF_STRING, QUIT_COMMAND, w!("Quit Syntra"))?;
        let mut position = POINT::default();
        GetCursorPos(&mut position)?;
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
            position.x,
            position.y,
            None,
            hwnd,
            None,
        );
        // Required by the notification-area menu contract so it dismisses correctly.
        let _ = PostMessageW(Some(hwnd), WM_APP, WPARAM(0), LPARAM(0));
        Ok(())
    })();
    if let Err(error) = menu_result {
        log::error!("Windows notification-area menu failed: {error}");
    }
    let _ = DestroyMenu(menu);
}

fn tray_error(error: WindowsError) -> PlatformError {
    PlatformError::Tray(error.to_string())
}
