use slint::{Color, ComponentHandle};

/// Where a snapshot is written: `SYNTRA_SMOKE_OUT` when set, otherwise the
/// system temporary directory. Never a fixed `/tmp`, so a run on a machine
/// with another temporary location (or a read-only `/tmp`) still works.
fn output_path(name: &str) -> String {
    let directory = std::env::var_os("SYNTRA_SMOKE_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&directory);
    directory
        .join(format!("syntra-{name}.ppm"))
        .display()
        .to_string()
}

fn snapshot(app: &syntra_ui::app::AppWindow, name: &str) {
    match app.window().take_snapshot() {
        Ok(image) => {
            let path = output_path(name);
            let mut data = format!("P6\n{} {}\n255\n", image.width(), image.height()).into_bytes();
            let mut min = [u8::MAX; 4];
            let mut max = [u8::MIN; 4];
            let mut opaque = 0usize;
            let mut transparent = 0usize;
            for pixel in image.as_slice() {
                let channels = [pixel.r, pixel.g, pixel.b, pixel.a];
                for (index, value) in channels.into_iter().enumerate() {
                    min[index] = min[index].min(value);
                    max[index] = max[index].max(value);
                }
                opaque += usize::from(pixel.a == u8::MAX);
                transparent += usize::from(pixel.a == 0);
                // PPM has no alpha channel: composite over white instead of
                // turning transparent pixels into the black diagnostic block.
                let alpha = u16::from(pixel.a);
                data.extend_from_slice(&[
                    ((u16::from(pixel.r) * alpha + 255 * (255 - alpha)) / 255) as u8,
                    ((u16::from(pixel.g) * alpha + 255 * (255 - alpha)) / 255) as u8,
                    ((u16::from(pixel.b) * alpha + 255 * (255 - alpha)) / 255) as u8,
                ]);
            }
            std::fs::write(&path, data).unwrap();
            println!(
                "SNAPSHOT {name} {}x{} rgba-min={min:?} rgba-max={max:?} opaque={opaque} transparent={transparent} path={path}",
                image.width(),
                image.height(),
            );
        }
        Err(error) => eprintln!("SNAPSHOT ERROR {name}: {error}"),
    }
}

/// Entries covering every card state the plugin manager can show, built from
/// the same Slint struct the dashboard fills from the daemon's report. No
/// daemon is involved: this checks how the page draws, not how the daemon
/// decides.
fn plugin_entries() -> Vec<syntra_ui::app::PluginEntry> {
    use slint::{ModelRc, VecModel};
    use syntra_ui::app::{PluginEntry, PluginRoleEntry};

    let role = |role: &str, active: bool, preferred: bool, unavailable: &str| PluginRoleEntry {
        role: role.into(),
        active,
        preferred,
        unavailable: unavailable.into(),
    };
    let base = |id: &str, name: &str| PluginEntry {
        id: id.into(),
        name: name.into(),
        description: "Copy files in your file manager and paste them on another device.".into(),
        version: "1.0.0".into(),
        author: "Example author".into(),
        license: "GPL-3.0-or-later".into(),
        executable: format!("/opt/syntra/plugins/{id}/bin/{id}").into(),
        installed: true,
        health: "stopped".into(),
        protocol_version: 1,
        supported_protocol_version: 1,
        state: "installed".into(),
        trust: "unapproved".into(),
        origin: "drop-in".into(),
        digest: "9f2c41b7a0d35e68".into(),
        args: "--serve".into(),
        manages_lifecycle: true,
        ..Default::default()
    };

    let mut bundled = base("clipboard", "File clipboard");
    bundled.bundled = true;
    bundled.enabled = true;
    bundled.running = true;
    bundled.health = "healthy".into();
    bundled.state = "ready".into();
    bundled.trust = "bundled".into();
    bundled.origin = "bundled".into();
    bundled.digest = "".into();
    bundled.pid = "4242".into();
    bundled.roles = ModelRc::new(VecModel::from(vec![role(
        "file-clipboard",
        true,
        false,
        "",
    )]));

    let mut unapproved = base("acme-files", "Acme files");
    unapproved.enabled = true;
    unapproved.can_approve = true;
    unapproved.needs_approval = true;
    unapproved.roles = ModelRc::new(VecModel::from(vec![role(
        "file-clipboard",
        false,
        false,
        "its content is not approved",
    )]));

    let mut approved = base("approved-files", "Approved files");
    approved.enabled = true;
    approved.running = true;
    approved.health = "healthy".into();
    approved.state = "ready".into();
    approved.trust = "approved".into();
    approved.approved_digest = "9f2c41b7a0d35e68".into();
    approved.can_revoke = true;
    approved.can_uninstall = true;
    approved.origin = "installed".into();
    approved.roles = ModelRc::new(VecModel::from(vec![role("file-clipboard", true, true, "")]));

    let mut changed = base("changed-files", "Changed files");
    changed.enabled = true;
    changed.trust = "changed".into();
    changed.changed_since_approval = true;
    changed.needs_approval = true;
    changed.can_approve = true;
    changed.approved_digest = "0000aaaa11112222".into();
    changed.error = "the plugin changed since it was approved; review and approve it again".into();

    let mut pending = base("pending-files", "Pending files");
    pending.pending = "approving".into();
    pending.origin = "installed".into();

    let mut failed = base("failed-files", "Failed files");
    failed.enabled = true;
    failed.health = "failed".into();
    failed.state = "failed".into();
    failed.trust = "approved".into();
    failed.error = "killed by signal 9".into();
    failed.diagnostics = "segmentation fault in renderer".into();
    failed.restarts = 3;

    let mut unsupported = base("future-files", "Future files");
    unsupported.installed = false;
    unsupported.state = "unsupported".into();
    unsupported.health = "not-installed".into();
    unsupported.error =
        "needs manifest version 7 and protocol version 1; this Syntra understands manifest 1"
            .into();

    let mut candidate = base("newcomer", "Newcomer");
    candidate.candidate = true;
    candidate.can_approve = false;
    candidate.review_path = "/home/user/plugins/newcomer".into();
    candidate.review_files = "bin/newcomer, lib/helper.so".into();

    // A daemon from before plugin approval reports none of the new fields, so
    // the card must show only the controls that daemon understands.
    let mut legacy = base("legacy", "Legacy plugin");
    legacy.manages_lifecycle = false;
    legacy.digest = "".into();
    legacy.enabled = true;
    legacy.running = true;
    legacy.health = "healthy".into();

    vec![
        bundled,
        unapproved,
        approved,
        changed,
        pending,
        failed,
        unsupported,
        candidate,
        legacy,
    ]
}

/// Shows the plugin manager with every card state and takes snapshots once the
/// frames have settled.
///
/// A snapshot taken before the launch overlay has gone, or before the model
/// has been laid out, shows the overlay or an empty page; so the overlay is
/// dismissed, a redraw is requested, and the snapshot waits for the next
/// frames. The renderer is the software one: this is a visual check of the
/// markup, not a claim about any device.
fn plugins_page(app: syntra_ui::app::AppWindow) -> Result<(), Box<dyn std::error::Error>> {
    use slint::{Model, ModelRc, VecModel};
    // The real English catalog, so the snapshot shows the words a user would
    // read, not message keys.
    let localizer = syntra_ui::localization::Localizer::new("en-US")?;
    app.global::<syntra_ui::app::Translations>()
        .on_translate(move |key, _| localizer.format(key.as_str(), None).into());
    let compact = std::env::var_os("SYNTRA_SMOKE_MOBILE").is_some();
    let (width, height) = if compact {
        (412.0, 915.0)
    } else {
        (1100.0, 900.0)
    };
    let theme = app.global::<syntra_ui::app::Theme>();
    if compact {
        theme.set_desktop_platform(false);
    }
    theme.set_window_preferred_width(width);
    theme.set_window_preferred_height(height);
    app.window()
        .set_size(slint::LogicalSize::new(width, height));
    app.show()?;
    app.window()
        .set_size(slint::LogicalSize::new(width, height));
    app.set_launch_ready(true);
    app.set_launch_visible(false);
    let state = app.global::<syntra_ui::app::AppState>();
    state.set_connected(true);
    let entries = plugin_entries();
    let entries = match std::env::var("SYNTRA_SMOKE_PLUGIN_INDEX") {
        Ok(index) => vec![
            entries
                .get(index.parse::<usize>()?)
                .ok_or("unknown plugin fixture")?
                .clone(),
        ],
        Err(_) => entries,
    };
    state.set_plugins(ModelRc::new(VecModel::from(entries)));
    app.set_selected_page("plugins".into());

    let weak = app.as_weak();
    // Two timers apart: the first lets the page be created and laid out, the
    // second runs after the redraw it requested has been painted.
    slint::Timer::single_shot(std::time::Duration::from_millis(600), move || {
        let app = weak.upgrade().unwrap();
        app.window().request_redraw();
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(400), move || {
            let app = weak.upgrade().unwrap();
            println!(
                "STATE launch-visible={} page={} plugins={}",
                app.get_launch_visible(),
                app.get_selected_page(),
                app.global::<syntra_ui::app::AppState>()
                    .get_plugins()
                    .row_count(),
            );
            snapshot(
                &app,
                if compact {
                    "plugins-compact"
                } else {
                    "plugins"
                },
            );
            slint::quit_event_loop().unwrap();
        });
    });
    app.run()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = syntra_ui::app::create_app()?;
    app.global::<syntra_ui::app::Translations>()
        .on_translate(|key, _| match key.as_str() {
            "welcome-title" | "app-title" => "Syntra".into(),
            "welcome-motto" => "Move freely. Stay connected.".into(),
            _ => key,
        });
    if std::env::var_os("SYNTRA_SMOKE_PLUGINS").is_some() {
        return plugins_page(app);
    }
    if std::env::var_os("SYNTRA_SMOKE_MOBILE").is_some() {
        let theme = app.global::<syntra_ui::app::Theme>();
        theme.set_desktop_platform(false);
        theme.set_window_preferred_width(412.0);
        theme.set_window_preferred_height(915.0);
        app.window().set_size(slint::LogicalSize::new(412.0, 915.0));
        app.show()?;
        app.window().set_size(slint::LogicalSize::new(412.0, 915.0));
        app.set_launch_ready(true);
        app.set_launch_visible(false);
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(900), move || {
            let app = weak.upgrade().unwrap();
            println!("MOBILE ACTUAL {:?}", app.window().size());
            app.window().request_redraw();
            snapshot(&app, "mobile-overview");
            app.set_selected_page("devices".into());
            let weak = app.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                let app = weak.upgrade().unwrap();
                snapshot(&app, "mobile-devices");
                app.set_selected_page("clipboard".into());
                let weak = app.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                    let app = weak.upgrade().unwrap();
                    snapshot(&app, "mobile-clipboard");
                    app.set_selected_page("settings".into());
                    let weak = app.as_weak();
                    slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                        snapshot(&weak.upgrade().unwrap(), "mobile-settings");
                        slint::quit_event_loop().unwrap();
                    });
                });
            });
        });
        app.run()?;
        return Ok(());
    }
    if std::env::var_os("SYNTRA_SMOKE_ABOUT").is_some() {
        app.set_selected_page("about".into());
    }
    let weak = app.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_millis(1500),
        move || {
            let app = weak.upgrade().unwrap();
            snapshot(&app, "welcome");
            app.set_launch_ready(true);

            let weak = app.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                let app = weak.upgrade().unwrap();
                snapshot(&app, "main-default");

                let theme = app.global::<syntra_ui::app::Theme>();
                theme.set_background(Color::from_rgb_u8(7, 18, 23));
                theme.set_surface(Color::from_rgb_u8(15, 30, 36));
                theme.set_elevated_surface(Color::from_rgb_u8(24, 43, 51));
                theme.set_header(Color::from_rgb_u8(10, 24, 29));
                theme.set_sidebar(Color::from_rgb_u8(12, 27, 33));
                theme.set_border(Color::from_rgb_u8(45, 72, 82));
                theme.set_text(Color::from_rgb_u8(237, 248, 250));
                theme.set_muted_text(Color::from_rgb_u8(165, 189, 195));
                theme.set_selection(Color::from_rgb_u8(33, 67, 77));
                theme.set_hover_surface(Color::from_rgb_u8(27, 53, 62));
                theme.set_pressed_surface(Color::from_rgb_u8(38, 70, 81));
                theme.set_accent(Color::from_rgb_u8(128, 199, 255));
                theme.set_base_mode("black".into());
                theme.set_palette("ice".into());

                let weak = app.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(100), move || {
                    let app = weak.upgrade().unwrap();
                    snapshot(&app, "main-dark-ice");

                    println!(
                        "STATE launch-visible={} mode={} palette={}",
                        app.get_launch_visible(),
                        app.global::<syntra_ui::app::Theme>().get_base_mode(),
                        app.global::<syntra_ui::app::Theme>().get_palette(),
                    );
                    let weak = app.as_weak();
                    slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
                        let app = weak.upgrade().unwrap();
                        snapshot(&app, "about");
                        app.window().set_size(slint::LogicalSize::new(380.0, 760.0));
                        let weak = app.as_weak();
                        slint::Timer::single_shot(
                            std::time::Duration::from_millis(200),
                            move || {
                                snapshot(&weak.upgrade().unwrap(), "about-compact");
                                slint::quit_event_loop().unwrap();
                            },
                        );
                    });
                });
            });
        },
    );
    app.run()?;
    Ok(())
}
