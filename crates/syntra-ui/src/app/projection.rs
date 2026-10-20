//! State to view: everything that turns the daemon's reported state and the
//! user's presentation settings into what the window shows.
//!
//! Nothing here sends a request or reacts to the user. A function takes the
//! window and a snapshot and writes properties and models; the callbacks that
//! react to input are in [`super::bindings`].

use super::plugins;
use super::*;

/// One diagnostic as a single plain-text line, for copying. It carries the
/// full date and time: a pasted line must stand on its own.
pub(super) fn diagnostic_line(entry: &DiagnosticEntry) -> String {
    [
        entry.stamp.as_str(),
        entry.level.as_str(),
        entry.stage.as_str(),
        entry.direction.as_str(),
        entry.correlation.as_str(),
        entry.message.as_str(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("  ")
}

/// Log records carry UTC RFC 3339 stamps; people read local wall time.
/// Returns `(day, time, full)`: `29.09`, `07:16:39.800` and
/// `2026-09-29 07:16:39.800`. The time always has the same width, so the
/// column lines up whatever day the record is from.
fn local_log_parts(stamp: &str) -> (String, String, String) {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(stamp) else {
        return (String::new(), stamp.to_owned(), stamp.to_owned());
    };
    let local = parsed.with_timezone(&chrono::Local);
    (
        local.format("%d.%m").to_string(),
        local.format("%H:%M:%S%.3f").to_string(),
        local.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
    )
}

pub(super) fn project_local_identity(app: &AppWindow, settings: &PresentationSettings) {
    let name = if settings.local_device.friendly_name.trim().is_empty() {
        hostname::get()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "Syntra".into())
    } else {
        settings.local_device.friendly_name.clone()
    };
    app.set_local_device_name(name.into());
    let image = (!settings.local_device.image_path.is_empty())
        .then(|| {
            slint::Image::load_from_path(std::path::Path::new(&settings.local_device.image_path))
                .map_err(|error| log::warn!("cannot load this device's image: {error}"))
                .ok()
        })
        .flatten();
    app.set_has_local_device_image(image.is_some());
    let image = image.unwrap_or_default();
    app.set_local_device_image(image);
}

fn hex_color(value: &str) -> Color {
    let raw = value.strip_prefix('#').unwrap_or(value);
    if raw.len() == 6 {
        if let Ok(rgb) = u32::from_str_radix(raw, 16) {
            return Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        }
    }
    Color::from_rgb_u8(113, 131, 85)
}

pub(super) fn apply_appearance(app: &AppWindow, settings: &PresentationSettings) {
    let state = app.global::<AppState>();
    state.set_suppress_popups(settings.suppress_popups);
    // The message column (last) cannot be hidden: a log without it is empty.
    let shown: Vec<bool> = (0..crate::settings::DIAGNOSTIC_COLUMNS)
        .map(|index| {
            index + 1 == crate::settings::DIAGNOSTIC_COLUMNS
                || settings.hidden_diagnostic_columns & (1 << index) == 0
        })
        .collect();
    state.set_diagnostic_columns(ModelRc::new(VecModel::from(shown)));
    let theme = app.global::<Theme>();
    let dark = matches!(settings.mode, crate::settings::BaseMode::Black);
    let (
        _background,
        surface,
        elevated,
        header,
        sidebar,
        border,
        text,
        muted,
        selection,
        hover,
        pressed,
    ) = match (settings.mode, settings.palette) {
        (crate::settings::BaseMode::White, crate::settings::Palette::Forest) => (
            "#ffffff", "#f5faf6", "#edf5ef", "#f8fcf9", "#eef7f0", "#c5d8c9", "#17261b", "#52695a",
            "#d8eadc", "#e7f2e9", "#dbe9de",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Ocean) => (
            "#ffffff", "#f4f9fc", "#eaf3f8", "#f8fbfd", "#ecf6fb", "#bfd3df", "#142630", "#4b6674",
            "#d3e8f3", "#e3f0f6", "#d5e7f0",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Slate) => (
            "#ffffff", "#f6f8fa", "#edf1f5", "#fafbfc", "#eef2f6", "#c7d0d9", "#1b232c", "#56636f",
            "#dce3ea", "#e8edf2", "#dce3e9",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Sand) => (
            "#ffffff", "#fbf8f1", "#f4eddf", "#fdfbf7", "#f7f0e3", "#ddcfb7", "#2a2115", "#74644d",
            "#eee0c6", "#f3ead9", "#e9dcc6",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Graphite) => (
            "#ffffff", "#f7f7f7", "#eeeeee", "#fbfbfb", "#f1f1f1", "#cccccc", "#202020", "#626262",
            "#dfdfdf", "#e9e9e9", "#dddddd",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Aubergine) => (
            "#ffffff", "#faf6fa", "#f3ebf3", "#fcf9fc", "#f7eef7", "#dac6da", "#29182a", "#705671",
            "#ead8ea", "#f1e5f1", "#e7d8e7",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Copper) => (
            "#ffffff", "#fbf7f4", "#f4ece6", "#fdfaf8", "#f8efe9", "#decbbf", "#2b1d16", "#755d50",
            "#ecdccf", "#f3e8e1", "#e9dacf",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Rose) => (
            "#ffffff", "#fcf6f8", "#f6eaf0", "#fef9fb", "#faedf2", "#e0c4cf", "#2d1821", "#795663",
            "#f0d7e0", "#f6e5eb", "#ecd6de",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Ice) => (
            "#ffffff", "#f3fafc", "#e8f4f7", "#f8fcfd", "#eaf7f9", "#bdd8de", "#14272c", "#4b6970",
            "#d0e9ee", "#e0f1f4", "#d2e7eb",
        ),
        (crate::settings::BaseMode::White, crate::settings::Palette::Army) => (
            "#ffffff", "#f7f8f2", "#eff1e6", "#fbfbf8", "#f2f4e9", "#cfd3bb", "#222617", "#646a4f",
            "#e1e5cf", "#ebeddf", "#dfe3d2",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Forest) => (
            "#07100a", "#101b13", "#19261d", "#0b160e", "#0d1810", "#304536", "#edf6ef", "#a9bbaa",
            "#274331", "#1e3024", "#294034",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Ocean) => (
            "#061018", "#0e1b24", "#172833", "#091620", "#0b1922", "#2c4655", "#edf6fa", "#a4b9c4",
            "#203f50", "#1b3240", "#254353",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Slate) => (
            "#090d12", "#121920", "#1c252e", "#0d1319", "#10171e", "#34424e", "#f0f4f7", "#a9b4bd",
            "#2b3945", "#222d36", "#2d3a45",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Sand) => (
            "#141009", "#201a11", "#2d2518", "#19140c", "#1d170e", "#4b3d27", "#faf3e7", "#c5b69d",
            "#47391f", "#382e1e", "#493b26",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Graphite) => (
            "#090909", "#151515", "#212121", "#0e0e0e", "#121212", "#3b3b3b", "#f3f3f3", "#b5b5b5",
            "#343434", "#292929", "#373737",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Aubergine) => (
            "#120912", "#201220", "#2e1b2f", "#170d17", "#1c101c", "#49324a", "#f8eef8", "#bea9bf",
            "#432a44", "#352238", "#472e49",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Copper) => (
            "#150c08", "#22150f", "#301f17", "#1a100b", "#1e120d", "#4e382d", "#faefe9", "#c5aea2",
            "#493024", "#39261d", "#4a3227",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Rose) => (
            "#15090e", "#231219", "#321c25", "#1b0d13", "#1f1017", "#50323d", "#fbeef3", "#c8a8b4",
            "#4c2936", "#3c202b", "#4e2c39",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Ice) => (
            "#071217", "#0f1e24", "#182b33", "#0a181d", "#0c1b21", "#2d4852", "#edf8fa", "#a5bdc3",
            "#21434d", "#1b353e", "#264651",
        ),
        (crate::settings::BaseMode::Black, crate::settings::Palette::Army) => (
            "#090b07", "#14180f", "#20261a", "#0e110a", "#11150d", "#3a4430", "#f1f4eb", "#b1b9a4",
            "#333e28", "#283121", "#36422b",
        ),
    };
    let (success, warning, danger) = if dark {
        ("#8bd5a5", "#f5c36b", "#f28b82")
    } else {
        ("#247a45", "#8a5a00", "#b83a3a")
    };
    theme.set_background(if dark {
        Color::from_rgb_u8(0, 0, 0)
    } else {
        Color::from_rgb_u8(255, 255, 255)
    });
    theme.set_surface(hex_color(surface));
    theme.set_elevated_surface(hex_color(elevated));
    theme.set_header(hex_color(header));
    theme.set_sidebar(hex_color(sidebar));
    theme.set_border(hex_color(border));
    theme.set_text(hex_color(text));
    theme.set_muted_text(hex_color(muted));
    theme.set_selection(hex_color(selection));
    theme.set_hover_surface(hex_color(hover));
    theme.set_pressed_surface(hex_color(pressed));
    theme.set_success(hex_color(success));
    theme.set_warning(hex_color(warning));
    theme.set_danger(hex_color(danger));
    theme.set_base_mode(if dark { "black".into() } else { "white".into() });
    theme.set_palette(
        format!("{:?}", settings.palette)
            .to_ascii_lowercase()
            .into(),
    );
    theme.set_accent(hex_color(&settings.accent));
    theme.set_titlebar(if settings.titlebar_color.is_empty() {
        hex_color(header)
    } else {
        hex_color(&settings.titlebar_color)
    });
    theme.set_motion_enabled(!settings.reduced_motion);
    let compact = matches!(settings.density, crate::settings::Density::Compact);
    let zoom = settings.zoom_percent as f32 / 100.0;
    theme.set_density_scale(if compact { 0.85 } else { 1.0 });
    theme.set_spacing((if compact { 12.0 } else { 16.0 }) * zoom);
    theme.set_small_spacing((if compact { 6.0 } else { 8.0 }) * zoom);
    theme.set_large_spacing((if compact { 24.0 } else { 32.0 }) * zoom);
    theme.set_content_gap(24.0 * zoom);
    theme.set_card_padding((if compact { 16.0 } else { 20.0 }) * zoom);
    theme.set_control_height((if compact { 40.0 } else { 44.0 }) * zoom);
    theme.set_row_height((if compact { 56.0 } else { 64.0 }) * zoom);
    theme.set_body_size(16.0 * zoom);
    theme.set_caption_size(14.0 * zoom);
    theme.set_title_size(26.0 * zoom);
    theme.set_desktop_platform(!cfg!(target_os = "android"));
    theme.set_sidebar_expanded_width(248.0 * zoom);
    theme.set_sidebar_collapsed_width(68.0 * zoom);
    theme.set_nav_item_height(48.0 * zoom);
    let state = app.global::<AppState>();
    state.set_zoom_percent(settings.zoom_percent.into());
    state.set_base_mode(if dark { "black".into() } else { "white".into() });
    state.set_palette(
        format!("{:?}", settings.palette)
            .to_ascii_lowercase()
            .into(),
    );
    state.set_accent(settings.accent.clone().into());
    state.set_titlebar_color(settings.titlebar_color.clone().into());
    state.set_density(if compact {
        "compact".into()
    } else {
        "comfortable".into()
    });
    state.set_reduced_motion(settings.reduced_motion);
}

/// Rows handed to the diagnostics view at once.
///
/// The list is a scrolling tail, so projecting more than a screenful plus
/// scrollback buys nothing and costs a full model rebuild per update.
const DIAGNOSTIC_VISIBLE_ROWS: usize = 400;

thread_local! {
    /// The live diagnostics model, kept across projections.
    ///
    /// Replacing the model on every update discards the view's row state and
    /// forces Slint to rebuild the entire list. Holding one model and
    /// appending to it lets a burst of log lines cost only the rows that are
    /// actually new.
    pub(super) static DIAGNOSTIC_MODEL: Rc<VecModel<DiagnosticEntry>> = Rc::new(VecModel::default());
    /// `(appended, revision)` observed at the last projection.
    static DIAGNOSTIC_CURSOR: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

pub(super) fn project_live_diagnostics(
    app: &AppWindow,
    store: &Arc<Mutex<crate::diagnostics::DiagnosticStore>>,
) {
    let Ok(store) = store.lock() else { return };
    let global = app.global::<AppState>();

    let (previous_appended, previous_revision) = DIAGNOSTIC_CURSOR.with(|cursor| cursor.get());
    let appended = store.appended();
    let revision = store.revision();
    // A filter change alters which records qualify, so appending would be
    // wrong; only an unchanged filter and a pure growth can be appended.
    let rebuild = revision != previous_revision || appended < previous_appended;
    let new_records = appended.saturating_sub(previous_appended) as usize;
    if !rebuild && new_records == 0 {
        return;
    }

    let rows = store
        .filtered(DIAGNOSTIC_VISIBLE_ROWS)
        .into_iter()
        .map(|entry| {
            let (date, time, stamp) = local_log_parts(&entry.timestamp);
            DiagnosticEntry {
                date: date.into(),
                timestamp: time.into(),
                stamp: stamp.into(),
                level: entry.level.into(),
                stage: entry.stage.into(),
                direction: entry.direction.into(),
                correlation: entry.correlation.into(),
                message: entry.message.into(),
            }
        })
        .collect::<Vec<_>>();

    DIAGNOSTIC_MODEL.with(|model| {
        if rebuild || new_records >= rows.len() {
            // Nothing in common with what is displayed; start again.
            model.set_vec(rows.clone());
        } else {
            // Append the tail, then trim the head back to the window so the
            // model never grows past what the view can show.
            for row in &rows[rows.len() - new_records..] {
                model.push(row.clone());
            }
            while model.row_count() > DIAGNOSTIC_VISIBLE_ROWS {
                model.remove(0);
            }
        }
        global.set_diagnostics(ModelRc::from(model.clone()));
    });
    DIAGNOSTIC_CURSOR.with(|cursor| cursor.set((appended, revision)));

    global.set_diagnostics_paused(store.filter.paused);
    global.set_diagnostics_error(
        store
            .last_error
            .clone()
            .unwrap_or_else(|| store.stream_status.clone())
            .into(),
    );
}

pub(super) fn project_locked_state(
    app: &AppWindow,
    state: &Arc<Mutex<AppViewState>>,
    settings: &Arc<Mutex<PresentationSettings>>,
) {
    if let Ok(view) = state.lock() {
        if let Ok(presentation) = settings.lock() {
            project_local_identity(app, &presentation);
            project_app_state(app, &view, &presentation);
        }
    }
}

pub(crate) fn device_presentation(
    app: &AppWindow,
    state: &AppViewState,
    origin: &str,
    recorded_label: Option<&str>,
) -> (SharedString, Option<slint::Image>) {
    let local = state.public_key_fingerprint.as_deref() == Some(origin);
    let client = app
        .global::<AppState>()
        .get_clients()
        .iter()
        .find(|client| client.fingerprint.as_str() == origin);
    let profile = state.peer_profiles.get(origin);
    let label = local
        .then(|| app.get_local_device_name().to_string())
        .or_else(|| {
            client
                .as_ref()
                .map(|client| client.display_name.to_string())
        })
        .filter(|label| !label.trim().is_empty() && label != origin)
        .or_else(|| {
            profile
                .map(|profile| profile.display_name.clone())
                .filter(|label| !label.trim().is_empty() && label != origin)
        })
        .or_else(|| {
            state
                .authorization
                .get(origin)
                .cloned()
                .filter(|label| !label.trim().is_empty() && label != origin)
        })
        .or_else(|| {
            recorded_label
                .filter(|label| !label.trim().is_empty() && *label != origin)
                .map(str::to_owned)
        })
        .map(Into::into)
        .unwrap_or_else(|| {
            let translations = app.global::<Translations>();
            translations
                .invoke_translate("history-unknown-device".into(), translations.get_revision())
        });
    let image = if local {
        app.get_has_local_device_image()
            .then(|| app.get_local_device_image())
    } else {
        client
            .filter(|client| client.has_custom_image)
            .map(|client| client.custom_image)
            .or_else(|| {
                profile
                    .and_then(|profile| profile.avatar.as_ref())
                    .map(|avatar| {
                        slint::Image::from_rgba8(
                            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                &avatar.rgba,
                                avatar.width,
                                avatar.height,
                            ),
                        )
                    })
            })
    };
    (label, image)
}

/// Key identifying one history record.
type HistoryKey = (String, u64);

thread_local! {
    /// Decoded thumbnails, keyed by record.
    ///
    /// Converting an image means copying its whole RGBA buffer. Doing that
    /// for every record on every projection made opening History stutter,
    /// even though the pixels never change: a record is immutable once
    /// received. Confined to the UI thread, which is the only place that
    /// projects.
    static THUMBNAIL_CACHE: std::cell::RefCell<std::collections::HashMap<HistoryKey, slint::Image>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Bound on cached thumbnails, so scrolling a long history cannot grow
/// without limit. Cleared wholesale rather than evicted one by one: the cost
/// is one re-decode of what is still on screen.
const THUMBNAIL_CACHE_LIMIT: usize = 128;

/// Returns the thumbnail for a record, decoding it at most once.
fn history_thumbnail(key: &HistoryKey, state: &AppViewState) -> Option<slint::Image> {
    THUMBNAIL_CACHE.with(|cache| {
        if let Some(image) = cache.borrow().get(key) {
            return Some(image.clone());
        }
        let image = state.history_images.get(key)?;
        let (width, height) = (image.width?, image.height?);
        let length = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        // Reject a payload whose declared size disagrees with its bytes: the
        // buffer constructor would otherwise read out of bounds.
        if length != image.bytes.len() || length > 32 * 1024 * 1024 {
            return None;
        }
        let decoded = slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
            &image.bytes,
            width,
            height,
        ));
        let mut cache = cache.borrow_mut();
        if cache.len() >= THUMBNAIL_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key.clone(), decoded.clone());
        Some(decoded)
    })
}

pub(super) fn project_history(app: &AppWindow, state: &AppViewState) {
    let global = app.global::<AppState>();
    global.set_history_error(
        state
            .history_clear_error
            .as_ref()
            .or(state.history_error.as_ref())
            .cloned()
            .unwrap_or_default()
            .into(),
    );
    let status = state
        .history_clear_result
        .map(|(affected, acknowledged)| {
            let translations = app.global::<Translations>();
            let removed = translations
                .invoke_translate("history-cleared-count".into(), translations.get_revision());
            let peers = translations.invoke_translate(
                "history-acknowledged-peers".into(),
                translations.get_revision(),
            );
            format!("{removed}: {affected} · {peers}: {acknowledged}")
        })
        .unwrap_or_default();
    global.set_history_status(status.into());
    global.set_history_busy(state.history_query_pending || state.history_clear_pending);
    global.set_history_has_more(
        state
            .history_page
            .as_ref()
            .is_some_and(|page| page.next_offset.is_some()),
    );
    let records = state
        .history_page
        .as_ref()
        .map(|page| page.records.as_slice())
        .unwrap_or_default();
    let mut sources = std::collections::BTreeMap::new();
    global.set_history_records(ModelRc::new(VecModel::from(
        records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let key = (
                    record.event_id.origin_device_id.clone(),
                    record.event_id.origin_sequence,
                );
                let thumbnail = history_thumbnail(&key, state);
                let (kind, preview) = match &record.preview {
                    syntra_api::HistoryPreview::Text { preview, .. } => {
                        ("clipboard-text", preview.clone())
                    }
                    syntra_api::HistoryPreview::Image { size_bytes, .. } => {
                        ("clipboard-images", format_bytes(*size_bytes))
                    }
                    syntra_api::HistoryPreview::Files {
                        names,
                        total_size_bytes,
                        ..
                    } => (
                        "clipboard-files",
                        format!("{}\n{}", names.join(", "), format_bytes(*total_size_bytes)),
                    ),
                };
                let (source_label, source_image) =
                    sources.entry(key.0.clone()).or_insert_with(|| {
                        device_presentation(app, state, &key.0, record.origin_label.as_deref())
                    });
                let group_header = if index == 0 {
                    if record.pinned { "Pinned" } else { "Recent" }
                } else if record.pinned && !records[index - 1].pinned {
                    "Pinned"
                } else if !record.pinned && records[index - 1].pinned {
                    "Recent"
                } else {
                    ""
                };
                HistoryItem {
                    origin: key.0.into(),
                    sequence: key.1.to_string().into(),
                    timestamp: chrono::DateTime::from_timestamp_millis(record.created_at_ms)
                        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                        .unwrap_or_default()
                        .into(),
                    source_label: source_label.clone(),
                    source_image: source_image.clone().unwrap_or_default(),
                    has_source_image: source_image.is_some(),
                    kind_label: app
                        .global::<Translations>()
                        .invoke_translate(kind.into(), app.global::<Translations>().get_revision()),
                    preview_text: preview.into(),
                    is_text: record.kind == syntra_api::HistoryKind::Text,
                    has_thumbnail: thumbnail.is_some(),
                    thumbnail: thumbnail.unwrap_or_default(),
                    pinned: record.pinned,
                    group_header: if group_header == "Pinned" {
                        app.global::<Translations>().invoke_translate(
                            "history-pinned-section".into(),
                            app.global::<Translations>().get_revision(),
                        )
                    } else if group_header == "Recent" {
                        app.global::<Translations>().invoke_translate(
                            "history-recent-section".into(),
                            app.global::<Translations>().get_revision(),
                        )
                    } else {
                        "".into()
                    },
                }
            })
            .collect::<Vec<_>>(),
    )));
}

fn format_uptime(seconds: u64, known: bool) -> String {
    if !known {
        return String::new();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min");
    }
    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if hours < 24 {
        return if remaining_minutes == 0 {
            format!("{hours} h")
        } else {
            format!("{hours} h {remaining_minutes} min")
        };
    }
    let days = hours / 24;
    let remaining_hours = hours % 24;
    if remaining_hours == 0 {
        format!("{days} d")
    } else {
        format!("{days} d {remaining_hours} h")
    }
}

pub(super) fn project_app_state(
    app: &AppWindow,
    state: &AppViewState,
    settings: &PresentationSettings,
) {
    let global = app.global::<AppState>();
    global.set_connected(state.status.connected);
    global.set_reconnecting(state.status.reconnecting);
    global.set_transport_status(transport_label(state.status.transport.clone()).into());
    global.set_capture_enabled(state.input_health.capture);
    global.set_input_sharing(state.input_sharing.unwrap_or(false));
    global.set_input_sharing_known(state.status.connected && state.input_sharing.is_some());
    let daemon = state.daemon.as_ref();
    global.set_daemon_known(daemon.is_some());
    global.set_daemon_version(daemon.map(|d| d.version.clone()).unwrap_or_default().into());
    global.set_daemon_origin(
        match daemon.map(|d| d.origin) {
            Some(syntra_api::DaemonOrigin::Service) => "service",
            Some(syntra_api::DaemonOrigin::Dashboard) => "dashboard",
            Some(syntra_api::DaemonOrigin::Manual) => "manual",
            None => "",
        }
        .into(),
    );
    global.set_daemon_pid(daemon.map(|d| d.pid.to_string()).unwrap_or_default().into());
    global.set_daemon_uptime(
        format_uptime(
            daemon.map(|d| d.uptime_seconds).unwrap_or(0),
            daemon.is_some(),
        )
        .into(),
    );
    global.set_daemon_executable(
        daemon
            .map(|d| d.executable.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_daemon_socket(daemon.map(|d| d.socket.clone()).unwrap_or_default().into());
    global.set_daemon_config_dir(
        daemon
            .map(|d| d.config_dir.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_daemon_port(i32::from(daemon.map(|d| d.port).unwrap_or(0)));
    // Enforced between the tiers, unlike plugins where a difference is only
    // noted: the dashboard and the service are one product and are expected
    // to be replaced together, so a mismatch explains symptoms that
    // otherwise point nowhere.
    global.set_daemon_build(
        daemon
            .map(|d| d.build_fingerprint.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_daemon_build_mismatch(
        daemon.is_some_and(|d| d.build_fingerprint != syntra_api::BUILD_FINGERPRINT),
    );
    global.set_daemon_capture_backend(
        daemon
            .and_then(|d| d.capture_backend.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_daemon_emulation_backend(
        daemon
            .and_then(|d| d.emulation_backend.clone())
            .unwrap_or_default()
            .into(),
    );
    let names = |names: Option<&Vec<String>>| {
        ModelRc::new(VecModel::from(
            names
                .into_iter()
                .flatten()
                .map(|name| SharedString::from(name.as_str()))
                .collect::<Vec<_>>(),
        ))
    };
    global.set_capture_backends(names(daemon.map(|d| &d.capture_backends)));
    global.set_emulation_backends(names(daemon.map(|d| &d.emulation_backends)));
    global.set_capture_backend_choice(
        daemon
            .and_then(|d| d.capture_backend_choice.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_emulation_backend_choice(
        daemon
            .and_then(|d| d.emulation_backend_choice.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_emulation_enabled(state.input_health.emulation);
    global.set_capture_supported(state.capabilities.capture);
    global.set_emulation_supported(state.capabilities.emulation);
    global.set_clipboard_text_enabled(state.clipboard.text);
    global.set_clipboard_image_enabled(state.clipboard.image);
    global.set_clipboard_files_enabled(state.clipboard.files);
    global.set_independent_pointers_enabled(state.independent_pointers);
    global.set_hop_bypass_enabled(state.hop_bypass);
    global.set_multi_hop_enabled(state.multi_hop);
    global.set_clipboard_files_supported(state.capabilities.clipboard_files);
    global.set_listen_port(i32::from(state.diagnostics.port));
    global.set_locale(app.global::<Translations>().get_locale());
    global.set_local_fingerprint(
        state
            .public_key_fingerprint
            .clone()
            .unwrap_or_default()
            .into(),
    );
    global.set_regenerated_fingerprint(
        state
            .pending_identity_fingerprint
            .clone()
            .unwrap_or_default()
            .into(),
    );
    global.set_pending_fingerprint(
        state
            .connection_attempt
            .as_ref()
            .map(|attempt| attempt.fingerprint.clone())
            .unwrap_or_default()
            .into(),
    );
    global.set_diagnostics_error(
        state
            .diagnostics
            .last_error
            .clone()
            .unwrap_or_default()
            .into(),
    );

    // Counted up front: the map centres each zone's stack of peers, which it
    // cannot do while still discovering how many that zone holds.
    let mut zone_totals = [0i32; 4];
    for client in state.clients.values() {
        let position = position_index(client.position);
        zone_totals[position as usize] = zone_totals[position as usize].saturating_add(1);
    }
    let mut zone_counts = [0i32; 4];
    let clients = state
        .clients
        .values()
        .map(|client| {
            let position = position_index(client.position);
            let zone_index = zone_counts[position as usize];
            let zone_total = zone_totals[position as usize];
            zone_counts[position as usize] = zone_index.saturating_add(1);
            let fingerprint = state
                .client_fingerprints
                .get(&client.handle)
                .cloned()
                .unwrap_or_default();
            let identity_key = if fingerprint.is_empty() {
                String::new()
            } else {
                format!("cert:{fingerprint}")
            };
            let remote_profile = state.peer_profiles.get(&fingerprint);
            let identity = settings.devices.get(&identity_key);
            let display_name = identity
                .map(|item| item.friendly_name.clone())
                .filter(|name| !name.trim().is_empty())
                .or_else(|| {
                    remote_profile
                        .map(|profile| profile.display_name.clone())
                        .filter(|name| !name.trim().is_empty())
                })
                .unwrap_or_else(|| client.hostname.clone().unwrap_or_default());
            let image = identity
                .filter(|item| !item.image_path.is_empty())
                .and_then(|item| {
                    slint::Image::load_from_path(std::path::Path::new(&item.image_path)).ok()
                })
                .or_else(|| {
                    remote_profile
                        .and_then(|profile| profile.avatar.as_ref())
                        .map(|avatar| {
                            slint::Image::from_rgba8(
                                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                    &avatar.rgba,
                                    avatar.width,
                                    avatar.height,
                                ),
                            )
                        })
                });
            let has_custom_image = image.is_some();
            let custom_image = image.unwrap_or_default();
            ClientItem {
                handle: client.handle.to_string().into(),
                hostname: client.hostname.clone().unwrap_or_default().into(),
                identity_key: identity_key.clone().into(),
                display_name: display_name.into(),
                custom_image,
                has_custom_image,
                fixed_ips: join_addresses(&client.fixed_ips).into(),
                port: i32::from(client.port),
                position,
                zone_index,
                zone_total,
                active: client.active,
                active_address: client
                    .active_addr
                    .map(|address| address.to_string())
                    .unwrap_or_default()
                    .into(),
                alive: state.status.connected && client.alive,
                remote_ready: state.status.connected && client.remote_ready,
                round_trip: client
                    .round_trip_us
                    .filter(|_| state.status.connected && client.alive)
                    .map(format_round_trip)
                    .unwrap_or_default()
                    .into(),
                dns_ips: join_addresses(&client.dns_ips).into(),
                ips: join_addresses(&client.ips).into(),
                has_pressed_keys: client.has_pressed_keys,
                resolving: client.resolving,
                peer_commit: client
                    .peer_commit
                    .map(format_peer_commit)
                    .unwrap_or_default()
                    .into(),
                authorized: state.authorization.contains_key(&fingerprint),
                fingerprint: fingerprint.into(),
            }
        })
        .collect::<Vec<_>>();
    // Updated row by row: replacing the model recreates every row, which
    // throws away the open editor and its unsaved text on each daemon event.
    sync_model(global.get_clients(), clients, |model| {
        global.set_clients(model)
    });
    global.set_peer_zone_depth(zone_counts.into_iter().max().unwrap_or(1).max(1));
    global.set_discovered_peers(ModelRc::new(VecModel::from(
        state
            .discovered_peers
            .iter()
            .map(|peer| DiscoveredPeerItem {
                id: peer.id.clone().into(),
                display_name: peer.display_name.clone().into(),
                addresses: peer
                    .addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
                    .into(),
                port: peer.port.into(),
                added: state.clients.values().any(|client| {
                    peer.addresses.iter().any(|address| {
                        client.fixed_ips.contains(address) || client.ips.contains(address)
                    })
                }),
            })
            .collect::<Vec<_>>(),
    )));

    let mut active = Vec::new();
    let mut completed = Vec::new();
    let mut failed = Vec::new();
    let mut cancelled = Vec::new();
    for item in state.transfers.values().map(transfer_item).chain(
        state
            .manual_transfers
            .values()
            .map(|transfer| crate::manual_ui::transfer_item(app, state, transfer)),
    ) {
        match item.state {
            TransferState::Active => active.push(item),
            TransferState::Completed => completed.push(item),
            TransferState::Cancelled => cancelled.push(item),
            TransferState::Failed => failed.push(item),
        }
    }
    global.set_active_transfers(ModelRc::new(VecModel::from(active)));
    global.set_completed_transfers(ModelRc::new(VecModel::from(completed)));
    global.set_failed_transfers(ModelRc::new(VecModel::from(failed)));
    global.set_cancelled_transfers(ModelRc::new(VecModel::from(cancelled)));
    crate::manual_ui::project(app, state);

    plugins::sync(
        &global,
        state
            .plugins
            .iter()
            .map(|plugin| plugins::entry(plugin, &settings.accent))
            .collect::<Vec<_>>(),
    );

    let mut keys = state
        .authorization
        .keys()
        .cloned()
        .map(Into::into)
        .collect::<Vec<slint::SharedString>>();
    keys.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    global.set_authorized_keys(ModelRc::new(VecModel::from(keys)));
    if state.navigation.page == "clipboard" {
        project_history(app, state);
    }
}

fn transfer_item(transfer: &ClipboardTransferStatus) -> TransferItem {
    let (state, error_message, cancellable) = match &transfer.state {
        ClipboardTransferState::Pending | ClipboardTransferState::Transferring => {
            (TransferState::Active, String::new(), true)
        }
        ClipboardTransferState::Completed => (TransferState::Completed, String::new(), false),
        ClipboardTransferState::Cancelled => (TransferState::Cancelled, String::new(), false),
        ClipboardTransferState::Failed(message) => (TransferState::Failed, message.clone(), false),
    };
    let progress = if transfer.total_bytes == 0 {
        0.0
    } else {
        (transfer.transferred_bytes as f64 / transfer.total_bytes as f64).clamp(0.0, 1.0) as f32
    };
    TransferItem {
        transfer_id: transfer.transfer_id.to_string().into(),
        file_id: transfer.file_id.to_string().into(),
        name: transfer.name.clone().into(),
        kind_label: file_kind(&transfer.name).into(),
        direction: match transfer.direction {
            ClipboardTransferDirection::Receiving => TransferDirection::Incoming,
            ClipboardTransferDirection::Sending => TransferDirection::Outgoing,
        },
        transferred_label: format_bytes(transfer.transferred_bytes).into(),
        total_label: format_bytes(transfer.total_bytes).into(),
        rate_label: if transfer.bytes_per_second == 0 {
            "".into()
        } else {
            format!("{}/s", format_bytes(transfer.bytes_per_second)).into()
        },
        progress,
        state,
        error_message: error_message.into(),
        cancellable,
        show_progress: true,
        ..Default::default()
    }
}

/// Brings `current` to `items` in place when it is a `VecModel`, touching
/// only rows that changed so row components (and their local state) survive.
fn sync_model<T: Clone + PartialEq + 'static>(
    current: ModelRc<T>,
    items: Vec<T>,
    replace: impl FnOnce(ModelRc<T>),
) {
    let Some(model) = current.as_any().downcast_ref::<VecModel<T>>() else {
        replace(ModelRc::new(VecModel::from(items)));
        return;
    };
    let shared = model.row_count().min(items.len());
    for (row, item) in items.iter().enumerate().take(shared) {
        if model.row_data(row).as_ref() != Some(item) {
            model.set_row_data(row, item.clone());
        }
    }
    for item in items.iter().skip(shared) {
        model.push(item.clone());
    }
    while model.row_count() > items.len() {
        model.remove(model.row_count() - 1);
    }
}

fn transport_label(transport: TransportLifecycle) -> &'static str {
    match transport {
        TransportLifecycle::Unavailable => "Unavailable",
        TransportLifecycle::Reconnecting => "Reconnecting",
        TransportLifecycle::Ready => "Ready",
    }
}

fn position_index(position: Position) -> i32 {
    match position {
        Position::Left => 0,
        Position::Right => 1,
        Position::Top => 2,
        Position::Bottom => 3,
    }
}

fn join_addresses(addresses: &[IpAddr]) -> String {
    addresses
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_peer_commit(commit: [u8; 8]) -> String {
    commit.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// "0.4 ms" below ten milliseconds, whole milliseconds above: sub-millisecond
/// detail matters on a wired LAN and is noise on Wi-Fi.
fn format_round_trip(micros: u32) -> String {
    if micros < 10_000 {
        format!("{:.1} ms", f64::from(micros) / 1000.0)
    } else {
        format!("{} ms", (micros + 500) / 1000)
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = bytes as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit + 1 < UNITS.len() {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

fn file_kind(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_uppercase())
        .filter(|extension| !extension.is_empty())
        .unwrap_or_else(|| "File".into())
}

#[cfg(test)]
mod sync_model_tests {
    use super::sync_model;
    use slint::{Model, ModelRc, VecModel};

    /// Rows must be updated in place so open editors survive daemon events.
    #[test]
    fn keeps_the_model_and_only_touches_changed_rows() {
        let model = ModelRc::new(VecModel::from(vec![1, 2, 3]));
        let mut replaced = false;
        sync_model(model.clone(), vec![1, 5, 3, 4], |_| replaced = true);
        assert!(!replaced);
        assert_eq!(model.iter().collect::<Vec<_>>(), vec![1, 5, 3, 4]);
        sync_model(model.clone(), vec![9], |_| replaced = true);
        assert!(!replaced);
        assert_eq!(model.iter().collect::<Vec<_>>(), vec![9]);
    }

    #[test]
    fn replaces_a_model_that_is_not_a_vec_model() {
        let mut replaced = None;
        sync_model(ModelRc::default(), vec![7], |m| replaced = Some(m));
        assert_eq!(replaced.unwrap().iter().collect::<Vec<_>>(), vec![7]);
    }
}
