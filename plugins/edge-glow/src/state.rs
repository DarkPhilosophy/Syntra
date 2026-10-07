//! What the plugin knows: its settings and the light on each edge.
//!
//! Everything the daemon says arrives as a [`Message`]; everything a renderer
//! needs is read back as a list of [`Strip`]s for one frame. There is no
//! window, clock or graphics call here, so the behaviour that matters (a
//! disabled glow is dark, a push that stops fades, a crossing replaces the
//! light on its edge) is tested without a desktop.

use std::time::{Duration, Instant};

use syntra_plugin_api::{Edge, Message, PointerEvent};

use crate::glow::{self, Light, Rgb};

/// The colour used until the user picks one: a bright cyan, which shows on a
/// dark screen where the olive green it replaced barely did. The same value,
/// as text, is the default in the plugin's manifest.
pub const DEFAULT_COLOUR: Rgb = (0.2, 0.8, 1.0);
/// How wide the lit strip is by default, in logical pixels.
pub const DEFAULT_WIDTH: u32 = 56;
/// The narrowest and widest strip a user may choose.
pub const WIDTH_RANGE: (u32, u32) = (16, 160);

/// How the glow reaches the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    /// Best available: the GPU where it works, the CPU where it does not.
    Auto,
    /// OpenGL (or GLES) on the GPU.
    OpenGl,
    /// The CPU, with no graphics driver involved.
    Software,
}

impl Renderer {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "opengl" => Some(Self::OpenGl),
            "software" => Some(Self::Software),
            _ => None,
        }
    }
}

/// The user's choices, as the daemon sends them.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Whether to draw anything at all.
    pub enabled: bool,
    /// The glow colour, ignored while `rainbow` is on.
    pub colour: Rgb,
    /// Cycle through hues instead of using one colour.
    pub rainbow: bool,
    /// Width of the lit strip in logical pixels.
    pub width: u32,
    /// How the glow is drawn.
    pub renderer: Renderer,
    /// Whether to also glow while a push builds, not only on entry.
    pub show_pressure: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            colour: DEFAULT_COLOUR,
            rainbow: false,
            width: DEFAULT_WIDTH,
            renderer: Renderer::Auto,
            show_pressure: true,
        }
    }
}

impl Settings {
    /// Applies `(key, value)` pairs on top of `self`.
    ///
    /// A key this plugin does not know, or a value it cannot read, leaves that
    /// setting as it was: the daemon validates values, but a plugin that
    /// trusted that blindly would crash on the day a newer daemon sent
    /// something older code never heard of.
    pub fn apply(&mut self, values: &[(String, String)]) {
        for (key, value) in values {
            match key.as_str() {
                "enabled" => {
                    if let Some(flag) = parse_flag(value) {
                        self.enabled = flag;
                    }
                }
                "colour" => {
                    if let Some(colour) = glow::parse_hex(value) {
                        self.colour = colour;
                    }
                }
                "rainbow" => {
                    if let Some(flag) = parse_flag(value) {
                        self.rainbow = flag;
                    }
                }
                "show_pressure" => {
                    if let Some(flag) = parse_flag(value) {
                        self.show_pressure = flag;
                    }
                }
                "width" => {
                    if let Ok(width) = value.parse::<u32>() {
                        self.width = width.clamp(WIDTH_RANGE.0, WIDTH_RANGE.1);
                    }
                }
                "renderer" => {
                    if let Some(renderer) = Renderer::parse(value) {
                        self.renderer = renderer;
                    }
                }
                _ => {}
            }
        }
    }
}

fn parse_flag(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// One edge's light and when it began.
#[derive(Debug, Clone, Copy)]
struct Active {
    kind: Kind,
    since: Instant,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Entry { along: f32 },
    Push { along: f32, level: f32 },
    Fading { along: f32, level: f32 },
}

/// One lit strip for a frame: which edge, with what light, in what colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Strip {
    /// The edge of the screen it lies along.
    pub edge: Edge,
    /// The light on it now.
    pub light: Light,
}

/// The plugin's whole state.
#[derive(Debug)]
pub struct State {
    /// The user's current choices.
    pub settings: Settings,
    edges: [Option<Active>; 4],
    /// Where along each edge a pointer last came in or was pushed, kept after
    /// the light itself is gone: a push reports no position of its own, and
    /// without this it fell back to the middle of the edge.
    last_along: [Option<f32>; 4],
}

impl Default for State {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            edges: [None; 4],
            last_along: [None; 4],
        }
    }
}

fn slot(edge: Edge) -> usize {
    match edge {
        Edge::Left => 0,
        Edge::Right => 1,
        Edge::Top => 2,
        Edge::Bottom => 3,
    }
}

const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

/// A position along an edge that is not a number or is off the edge is the
/// middle of it, rather than a light drawn somewhere meaningless.
fn along_or_middle(along: Option<f32>) -> f32 {
    match along {
        Some(value) if value.is_finite() => value.clamp(0.0, 1.0),
        _ => 0.5,
    }
}

impl State {
    /// Applies one message from the daemon at time `now`.
    ///
    /// Anything that is not for this plugin is ignored, never an error: the
    /// protocol grows, and an older plugin must keep running beside a newer
    /// daemon.
    pub fn handle(&mut self, message: Message, now: Instant) {
        match message {
            Message::Settings { values } => {
                self.settings.apply(&values);
                if !self.settings.enabled {
                    self.edges = [None; 4];
                }
            }
            Message::Pointer(event) => self.pointer(event, now),
            _ => {}
        }
    }

    fn pointer(&mut self, event: PointerEvent, now: Instant) {
        if !self.settings.enabled {
            return;
        }
        match event {
            PointerEvent::Entered { edge, along, .. } => {
                // A new crossing replaces whatever light was on that edge.
                let place = along_or_middle(along);
                self.last_along[slot(edge)] = Some(place);
                self.edges[slot(edge)] = Some(Active {
                    kind: Kind::Entry { along: place },
                    since: now,
                });
            }
            PointerEvent::Left { edge, .. } => {
                // The pointer has gone, so what depended on it being there (a
                // push) goes with it. An entry is short and plays out: wiping
                // it made a quick in-and-out crossing show nothing at all.
                let index = slot(edge);
                if !matches!(
                    self.edges[index],
                    Some(Active {
                        kind: Kind::Entry { .. },
                        ..
                    })
                ) {
                    self.edges[index] = None;
                }
            }
            PointerEvent::Pressure {
                edge,
                along,
                amount,
            } => {
                if !self.settings.show_pressure {
                    return;
                }
                let level = if amount.is_finite() {
                    amount.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let index = slot(edge);
                if level <= 0.0 {
                    // The push is over. Whatever it had built fades out.
                    if let Some(Active {
                        kind: Kind::Push { along, level },
                        ..
                    }) = self.edges[index]
                    {
                        self.edges[index] = Some(Active {
                            kind: Kind::Fading { along, level },
                            since: now,
                        });
                    }
                } else if !matches!(
                    self.edges[index],
                    Some(Active {
                        kind: Kind::Entry { .. },
                        ..
                    })
                ) {
                    // An entry glow still playing is not cut short by a push.
                    // A push that reports no position keeps the one this edge
                    // last had (where the pointer came in), and only falls back
                    // to the middle if there never was one.
                    let kept = match self.edges[index] {
                        Some(Active {
                            kind:
                                Kind::Push { along, .. }
                                | Kind::Fading { along, .. }
                                | Kind::Entry { along },
                            ..
                        }) => Some(along),
                        None => self.last_along[index],
                    };
                    let place = match along {
                        Some(_) => along_or_middle(along),
                        None => kept.unwrap_or(0.5),
                    };
                    self.last_along[index] = Some(place);
                    self.edges[index] = Some(Active {
                        kind: Kind::Push {
                            along: place,
                            level,
                        },
                        since: now,
                    });
                }
            }
        }
    }

    /// The strips to draw at `now`, dropping any light that has finished.
    ///
    /// Empty means nothing is lit, which is when a renderer should stop
    /// redrawing and wait for the next message instead of spinning.
    pub fn frame(&mut self, now: Instant) -> Vec<Strip> {
        let mut strips = Vec::new();
        if !self.settings.enabled {
            return strips;
        }
        for edge in EDGES {
            let index = slot(edge);
            let Some(active) = self.edges[index] else {
                continue;
            };
            let age = now.saturating_duration_since(active.since);
            let light = match active.kind {
                Kind::Entry { along } => Light::Entry { along, age },
                Kind::Push { along, level } => Light::Push { along, level },
                Kind::Fading { along, level } => Light::Fading {
                    along,
                    level,
                    since: age,
                },
            };
            if light.is_over() {
                self.edges[index] = None;
                continue;
            }
            strips.push(Strip { edge, light });
        }
        strips
    }

    /// How long a renderer may sleep before it must draw again: not at all
    /// while something is lit, and until the next message otherwise.
    pub fn next_wake(&self) -> Option<Duration> {
        self.edges
            .iter()
            .any(Option::is_some)
            .then_some(Duration::from_millis(16))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entered(edge: Edge, along: Option<f32>) -> Message {
        Message::Pointer(PointerEvent::Entered {
            edge,
            along,
            peer: "phone".into(),
        })
    }

    fn pressure(edge: Edge, amount: f32) -> Message {
        Message::Pointer(PointerEvent::Pressure {
            edge,
            along: Some(0.4),
            amount,
        })
    }

    #[test]
    fn nothing_is_lit_until_something_happens() {
        let mut state = State::default();
        assert!(state.frame(Instant::now()).is_empty());
        assert_eq!(state.next_wake(), None, "an idle plugin must not spin");
    }

    #[test]
    fn an_entry_lights_its_edge_then_finishes() {
        let start = Instant::now();
        let mut state = State::default();
        state.handle(entered(Edge::Left, Some(0.3)), start);

        let strips = state.frame(start + Duration::from_millis(100));
        assert_eq!(strips.len(), 1);
        assert_eq!(strips[0].edge, Edge::Left);
        assert!(state.next_wake().is_some(), "an animation needs frames");

        let after = state.frame(start + glow::ENTRY_DURATION + Duration::from_millis(1));
        assert!(after.is_empty(), "a finished glow is dropped");
        assert_eq!(state.next_wake(), None, "and the plugin goes back to sleep");
    }

    #[test]
    fn a_disabled_glow_is_dark_and_stays_dark() {
        let start = Instant::now();
        let mut state = State::default();
        state.handle(entered(Edge::Top, None), start);
        state.handle(
            Message::Settings {
                values: vec![("enabled".into(), "false".into())],
            },
            start,
        );
        assert!(state.frame(start).is_empty(), "switching off clears it");

        state.handle(entered(Edge::Top, None), start);
        assert!(state.frame(start).is_empty(), "and nothing relights it");
    }

    #[test]
    fn a_push_builds_then_fades_when_it_stops() {
        let start = Instant::now();
        let mut state = State::default();
        state.handle(pressure(Edge::Right, 0.6), start);
        assert!(matches!(
            state.frame(start)[0].light,
            Light::Push { level, .. } if (level - 0.6).abs() < 1e-6
        ));

        state.handle(pressure(Edge::Right, 0.0), start);
        assert!(matches!(state.frame(start)[0].light, Light::Fading { .. }));
        let later = start + glow::RELEASE_DURATION + Duration::from_millis(1);
        assert!(state.frame(later).is_empty());
    }

    #[test]
    fn a_push_does_not_cut_an_entry_short() {
        let start = Instant::now();
        let mut state = State::default();
        state.handle(entered(Edge::Left, Some(0.5)), start);
        state.handle(pressure(Edge::Left, 0.9), start);
        assert!(matches!(state.frame(start)[0].light, Light::Entry { .. }));
    }

    fn left(edge: Edge) -> Message {
        Message::Pointer(PointerEvent::Left {
            edge,
            along: None,
            peer: "p".into(),
        })
    }

    /// Leaving ends what depends on the pointer being there (a push), on that
    /// edge only.
    #[test]
    fn leaving_ends_a_push_on_that_edge_only() {
        let start = Instant::now();
        let mut state = State::default();
        state.handle(pressure(Edge::Left, 0.8), start);
        state.handle(pressure(Edge::Right, 0.8), start);
        state.handle(left(Edge::Left), start);
        let strips = state.frame(start);
        assert_eq!(strips.len(), 1);
        assert_eq!(strips[0].edge, Edge::Right);
    }

    /// An entry is short and plays out: a crossing that is followed at once by
    /// leaving again used to wipe it, which looked like the glow not working.
    #[test]
    fn leaving_does_not_cut_an_entry_short() {
        let start = Instant::now();
        let mut state = State::default();
        state.handle(entered(Edge::Left, Some(0.3)), start);
        state.handle(left(Edge::Left), start + Duration::from_millis(100));
        let strips = state.frame(start + Duration::from_millis(200));
        assert_eq!(strips.len(), 1, "the entry is still playing");
        assert!(matches!(strips[0].light, Light::Entry { .. }));
        let later = start + glow::ENTRY_DURATION + Duration::from_millis(1);
        assert!(state.frame(later).is_empty(), "and it still ends by itself");
    }

    /// A push reports no position of its own; it must not drag the light back
    /// to the middle of the edge from where the pointer came in.
    #[test]
    fn a_push_without_a_position_keeps_the_place_of_the_entry() {
        let start = Instant::now();
        let mut state = State::default();
        state.handle(entered(Edge::Left, Some(0.2)), start);
        // The entry has played out, then the pointer is pushed against the edge.
        let after = start + glow::ENTRY_DURATION + Duration::from_millis(50);
        let _ = state.frame(after);
        state.handle(
            Message::Pointer(PointerEvent::Pressure {
                edge: Edge::Left,
                along: None,
                amount: 0.7,
            }),
            after,
        );
        let Light::Push { along, .. } = state.frame(after)[0].light else {
            panic!("expected a push");
        };
        assert_eq!(along, 0.2, "kept where the pointer came in, not the middle");
    }

    #[test]
    fn pressure_can_be_turned_off_without_losing_entries() {
        let start = Instant::now();
        let mut state = State::default();
        state.handle(
            Message::Settings {
                values: vec![("show_pressure".into(), "false".into())],
            },
            start,
        );
        state.handle(pressure(Edge::Bottom, 0.8), start);
        assert!(state.frame(start).is_empty());
        state.handle(entered(Edge::Bottom, None), start);
        assert_eq!(state.frame(start).len(), 1);
    }

    #[test]
    fn settings_are_applied_and_unreadable_ones_are_ignored() {
        let mut settings = Settings::default();
        settings.apply(&[
            ("colour".into(), "#ff8800".into()),
            ("rainbow".into(), "true".into()),
            ("width".into(), "9999".into()),
            ("renderer".into(), "software".into()),
        ]);
        assert_eq!(settings.colour, (1.0, 136.0 / 255.0, 0.0));
        assert!(settings.rainbow);
        assert_eq!(settings.width, WIDTH_RANGE.1, "clamped, not rejected");
        assert_eq!(settings.renderer, Renderer::Software);

        let before = settings.clone();
        settings.apply(&[
            ("colour".into(), "orange".into()),
            ("renderer".into(), "vulkan9000".into()),
            ("width".into(), "wide".into()),
            ("from_the_future".into(), "yes".into()),
        ]);
        assert_eq!(settings, before, "nothing it cannot read changes anything");
    }

    #[test]
    fn a_malformed_position_lights_the_middle_not_nonsense() {
        let start = Instant::now();
        let mut state = State::default();
        state.handle(entered(Edge::Top, Some(f32::NAN)), start);
        let Light::Entry { along, .. } = state.frame(start)[0].light else {
            panic!("expected an entry");
        };
        assert_eq!(along, 0.5);
    }
}
