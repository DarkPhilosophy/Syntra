//! Renders the diagnostics page offscreen with sample records and writes a
//! PPM to `/tmp`, so layout changes can be checked without a device.
//!
//! `cargo run -p syntra-ui --example diagnostics_preview` for the desktop
//! layout, with `SYNTRA_PREVIEW_MOBILE=1` for a phone. `SYNTRA_PREVIEW_CLICKS`
//! takes `x,y;x,y` logical positions to click before the snapshot (open the
//! detail panel or the column chooser), and `SYNTRA_PREVIEW_HIDE` a
//! comma-separated list of column indexes to hide.
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, VecModel};
use syntra_ui::app::{AppState, DiagnosticEntry, Theme};

fn entry(
    time: &str,
    level: &str,
    stage: &str,
    direction: &str,
    event: &str,
    message: &str,
) -> DiagnosticEntry {
    DiagnosticEntry {
        date: "29.09".into(),
        timestamp: time.into(),
        stamp: format!("2026-09-29 {time}").into(),
        level: level.into(),
        stage: stage.into(),
        direction: direction.into(),
        correlation: event.into(),
        message: message.into(),
    }
}

fn click(app: &syntra_ui::app::AppWindow, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    let window = app.window();
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = syntra_ui::app::create_app()?;
    let mobile = std::env::var_os("SYNTRA_PREVIEW_MOBILE").is_some();
    let (width, height) = if mobile {
        (412.0, 915.0)
    } else {
        (1100.0, 720.0)
    };
    app.global::<syntra_ui::app::Translations>()
        .on_translate(|key, _| {
            match key.as_str() {
                "navigation-diagnostics" => "Diagnostics",
                "diagnostics-filter" => "Filter messages",
                "diagnostics-live" => "Live",
                "diagnostics-following-tail" => "Following tail",
                "diagnostics-follow-tail" => "Follow tail",
                "diagnostics-pause" => "Pause",
                "diagnostics-columns" => "Columns",
                "diagnostics-copy-all" => "Copy all",
                "diagnostics-clear" => "Clear",
                "diagnostics-copy" => "Copy",
                "action-close" => "Close",
                "diagnostics-column-date" => "Date",
                "diagnostics-column-time" => "Time",
                "diagnostics-filter-level" => "Level",
                "diagnostics-filter-stage" => "Stage",
                "diagnostics-filter-direction" => "Direction",
                "diagnostics-column-event" => "Event",
                "diagnostics-column-message" => "Message",
                _ => return key,
            }
            .into()
        });
    let theme = app.global::<Theme>();
    if mobile {
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
    app.set_selected_page("diagnostics".into());
    let rows = vec![
        entry(
            "07:14:53.913",
            "warning",
            "network",
            "local",
            "pointers",
            "independent pointers refused: Independent pointers need a GNOME setting that takes effect at the next login (direct scanout off, avoiding a GNOME Shell crash with extra cursors). It has been installed: log out and back in.",
        ),
        entry(
            "07:16:39.725",
            "info",
            "network",
            "incoming",
            "entry",
            "accepting entry from 10.0.0.3:4242",
        ),
        entry(
            "07:16:39.800",
            "info",
            "capture",
            "outgoing",
            "enter",
            "entering client 1 ...",
        ),
        entry(
            "07:16:39.831",
            "info",
            "capture",
            "incoming",
            "ack",
            "client 1 acknowledged entry",
        ),
        entry(
            "07:16:40.002",
            "error",
            "network",
            "outgoing",
            "connect-failed",
            "failed to connect to 10.0.0.3:4242: `Connection timed out`",
        ),
        entry(
            "07:16:41.410",
            "info",
            "clipboard",
            "outgoing",
            "clipboard-share",
            "sharing copied text (12 bytes) with 2 device(s)",
        ),
        entry(
            "07:16:42.100",
            "info",
            "network",
            "local",
            "shutdown",
            "terminating service ...",
        ),
    ];
    let state = app.global::<AppState>();
    state.set_diagnostics(ModelRc::new(VecModel::from(rows)));
    let hidden: Vec<usize> = std::env::var("SYNTRA_PREVIEW_HIDE")
        .unwrap_or_default()
        .split(',')
        .filter_map(|index| index.trim().parse().ok())
        .collect();
    let shown: Vec<bool> = (0..7).map(|index| !hidden.contains(&index)).collect();
    state.set_diagnostic_columns(ModelRc::new(VecModel::from(shown)));
    let clicks: Vec<(f32, f32)> = std::env::var("SYNTRA_PREVIEW_CLICKS")
        .unwrap_or_default()
        .split(';')
        .filter_map(|pair| {
            let (x, y) = pair.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        })
        .collect();

    let weak = app.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(600), move || {
        let app = weak.upgrade().unwrap();
        for (x, y) in clicks {
            click(&app, x, y);
        }
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(400), move || {
            let app = weak.upgrade().unwrap();
            match app.window().take_snapshot() {
                Ok(image) => {
                    let name = if mobile { "mobile" } else { "desktop" };
                    let path = format!("/tmp/syntra-diagnostics-{name}.ppm");
                    let mut data =
                        format!("P6\n{} {}\n255\n", image.width(), image.height()).into_bytes();
                    for pixel in image.as_slice() {
                        // No alpha in PPM: composite over the page colour.
                        let alpha = u16::from(pixel.a);
                        for value in [pixel.r, pixel.g, pixel.b] {
                            data.push(
                                ((u16::from(value) * alpha + 16 * (255 - alpha)) / 255) as u8,
                            );
                        }
                    }
                    std::fs::write(&path, data).unwrap();
                    println!("WROTE {path}");
                }
                Err(error) => eprintln!("SNAPSHOT ERROR: {error}"),
            }
            slint::quit_event_loop().ok();
        });
    });
    app.run()?;
    Ok(())
}
