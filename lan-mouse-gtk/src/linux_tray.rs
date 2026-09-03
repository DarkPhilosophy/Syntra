use gtk::glib;
use gtk::prelude::*;
use ksni::blocking::TrayMethods;

#[derive(Debug)]
struct LanMouseTray {
    app: glib::SendWeakRef<adw::Application>,
}

impl ksni::Tray for LanMouseTray {
    fn id(&self) -> String {
        "de.feschber.LanMouse".into()
    }

    fn title(&self) -> String {
        "Lan Mouse".into()
    }

    fn icon_name(&self) -> String {
        "de.feschber.LanMouse".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        activate(&self.app, false);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;

        vec![
            StandardItem {
                label: "Open Lan Mouse".into(),
                icon_name: "de.feschber.LanMouse".into(),
                activate: Box::new(|tray: &mut Self| activate(&tray.app, false)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| activate(&tray.app, true)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn activate(app: &glib::SendWeakRef<adw::Application>, quit: bool) {
    let app = app.clone();
    glib::MainContext::default().invoke(move || {
        if let Some(app) = app.upgrade() {
            if quit {
                app.quit();
            } else {
                app.activate();
            }
        }
    });
}

pub(crate) fn setup(app: &adw::Application) {
    let tray = LanMouseTray {
        app: app.downgrade().into(),
    };
    std::thread::Builder::new()
        .name("lan-mouse-tray-startup".into())
        .spawn(move || {
            if let Err(error) = tray.spawn() {
                log::error!("failed to create system tray indicator: {error}");
            }
        })
        .expect("failed to start system tray thread");
}
