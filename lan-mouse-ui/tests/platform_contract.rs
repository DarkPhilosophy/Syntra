use std::sync::{Arc, Mutex};

use lan_mouse_ui::platform::{
    CloseBehavior, PlatformAction, PlatformCallbacks, PlatformCapabilities, PlatformError,
};

#[test]
fn callbacks_dispatch_frontend_lifecycle_events() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let shown = Arc::clone(&events);
    let quit = Arc::clone(&events);
    let callbacks = PlatformCallbacks::new(
        move || shown.lock().unwrap().push("show"),
        move || quit.lock().unwrap().push("quit"),
    );

    callbacks.show();
    callbacks.quit();

    assert_eq!(*events.lock().unwrap(), ["show", "quit"]);
}

#[test]
fn platform_contract_describes_background_and_permission_capabilities() {
    let capabilities = PlatformCapabilities {
        tray: true,
        close_behavior: CloseBehavior::Hide,
        flatpak_permissions: true,
    };

    assert!(capabilities.tray);
    assert!(capabilities.flatpak_permissions);
    assert_eq!(capabilities.close_behavior, CloseBehavior::Hide);

    let action = PlatformAction::GrantFlatpakFilesystemAccess {
        application_id: "org.kde.dolphin".into(),
    };
    assert!(matches!(
        action,
        PlatformAction::GrantFlatpakFilesystemAccess { application_id }
            if application_id == "org.kde.dolphin"
    ));
    assert!(matches!(
        PlatformError::UnsupportedAction("permission settings"),
        PlatformError::UnsupportedAction("permission settings")
    ));
}
