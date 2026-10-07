//! The state of the glow along a screen edge, apart from where it is drawn.
//!
//! Two things light an edge. A pointer that comes in from another device
//! lights it from the place it entered, spreading along the edge and fading.
//! A pointer being pushed out lights it in proportion to the push, from where
//! the pointer is, until it fills the edge at the moment it passes on. Both
//! are reduced here to one question a renderer asks every frame: how bright
//! is each point of each edge, and in what colour. Nothing in this module
//! draws, so it holds on any desktop and can be tested without one.

use std::time::Duration;

use syntra_api::Position;

/// How long an entry glow takes to spread and fade away.
pub(crate) const ENTRY_DURATION: Duration = Duration::from_millis(900);
/// How long a glow built by pressure takes to fade once the push is over.
pub(crate) const RELEASE_DURATION: Duration = Duration::from_millis(350);
/// Share of the edge a glow covers at its start, either side of its centre.
const SEED_HALF_WIDTH: f32 = 0.04;

/// The kind of light on one edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Light {
    /// A pointer arrived at `along`; `age` is how long ago.
    Entry { along: f32, age: Duration },
    /// A pointer is being pushed out at `along` with `level` of the force
    /// needed to pass on, from 0.0 to 1.0.
    Push { along: f32, level: f32 },
    /// A push that has just ended, fading from `level`.
    Fading {
        along: f32,
        level: f32,
        since: Duration,
    },
}

/// Brightness, from 0.0 to 1.0, at `point` along an edge (0.0 to 1.0).
///
/// An entry opens from its centre and fades; a push widens with its level
/// until the whole edge is lit; a push that ended fades back.
pub(crate) fn intensity(light: Light, point: f32) -> f32 {
    let point = unit(point);
    match light {
        Light::Entry { along, age } => {
            let progress = (age.as_secs_f32() / ENTRY_DURATION.as_secs_f32()).clamp(0.0, 1.0);
            if progress >= 1.0 {
                return 0.0;
            }
            // Spreads to cover the edge, and dims as it does.
            let half = SEED_HALF_WIDTH + progress * 1.2;
            let fade = (1.0 - progress).powi(2);
            falloff(point, along, half) * fade
        }
        Light::Push { along, level } => {
            let level = unit(level);
            // At full force the glow reaches both ends whatever `along` is.
            let half = SEED_HALF_WIDTH + level * 1.2;
            falloff(point, along, half) * (0.25 + 0.75 * level)
        }
        Light::Fading {
            along,
            level,
            since,
        } => {
            let progress = (since.as_secs_f32() / RELEASE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
            let level = unit(level);
            let half = SEED_HALF_WIDTH + level * 1.2;
            falloff(point, along, half) * (0.25 + 0.75 * level) * (1.0 - progress)
        }
    }
}

/// A value held to 0.0..=1.0, where anything that is not a number is 0.0.
/// `NaN.clamp` stays `NaN`, and one such value would turn every pixel of the
/// glow into `NaN` for as long as it lasted.
fn unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 1.0 at `centre`, down to 0.0 `half` away, smooth in between.
fn falloff(point: f32, centre: f32, half: f32) -> f32 {
    // A centre that is not a number is the middle of the edge.
    let centre = if centre.is_finite() {
        unit(centre)
    } else {
        0.5
    };
    let distance = (point - centre).abs();
    if !half.is_finite() || half <= 0.0 || distance >= half {
        return 0.0;
    }
    let t = 1.0 - distance / half;
    // Smoothstep: no visible edge where the glow ends.
    t * t * (3.0 - 2.0 * t)
}

/// A colour as red, green and blue from 0.0 to 1.0.
pub(crate) type Rgb = (f32, f32, f32);

/// The colour of the glow now. `base` is the chosen colour; with `rainbow`
/// the hue turns with `clock` (seconds) instead, and `along` shifts it so the
/// glow is not one flat colour end to end.
pub(crate) fn colour(base: Rgb, rainbow: bool, clock: f32, along: f32) -> Rgb {
    if !rainbow {
        return base;
    }
    let hue = (clock * 0.35 + along.clamp(0.0, 1.0) * 0.5).rem_euclid(1.0);
    hsv(hue, 0.85, 1.0)
}

/// Hue in 0.0..1.0 to red, green and blue, at the given saturation and value.
fn hsv(hue: f32, saturation: f32, value: f32) -> Rgb {
    let sector = (hue.rem_euclid(1.0) * 6.0).floor();
    let fraction = hue.rem_euclid(1.0) * 6.0 - sector;
    let p = value * (1.0 - saturation);
    let q = value * (1.0 - saturation * fraction);
    let t = value * (1.0 - saturation * (1.0 - fraction));
    match sector as i32 % 6 {
        0 => (value, t, p),
        1 => (q, value, p),
        2 => (p, value, t),
        3 => (p, q, value),
        4 => (t, p, value),
        _ => (value, p, q),
    }
}

/// Parses `#rrggbb` into a colour; anything else is `None`, so a stored value
/// that is empty or malformed falls back to the accent.
pub(crate) fn parse_hex(value: &str) -> Option<Rgb> {
    let digits = value.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&digits[range], 16).map(|byte| f32::from(byte) / 255.0)
    };
    Some((
        channel(0..2).ok()?,
        channel(2..4).ok()?,
        channel(4..6).ok()?,
    ))
}

/// Writes a colour as `#rrggbb`, the form a setting is saved in.
pub(crate) fn to_hex(red: u8, green: u8, blue: u8) -> String {
    format!("#{red:02x}{green:02x}{blue:02x}")
}

/// The colour to show for a colour setting whose value is `value`: the value
/// itself when it is a real colour, otherwise `fallback` (the accent), which
/// is what an empty value means.
pub(crate) fn setting_colour(value: &str, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match parse_hex(value) {
        Some((red, green, blue)) => (
            (red * 255.0).round() as u8,
            (green * 255.0).round() as u8,
            (blue * 255.0).round() as u8,
        ),
        None => fallback,
    }
}

/// Which side of the screen glows for a pointer that entered through `edge`
/// of the layout, as the side of this screen itself.
pub(crate) fn screen_side(edge: Position) -> Position {
    edge
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// An entry lights the place it came in and nowhere near the far end.
    #[test]
    fn an_entry_starts_at_the_place_it_came_in() {
        let light = Light::Entry {
            along: 0.25,
            age: at(0),
        };
        assert!(intensity(light, 0.25) > 0.99, "brightest at the entry");
        assert!(intensity(light, 0.40) < 0.01, "dark away from it at first");
        assert_eq!(intensity(light, 0.9), 0.0);
    }

    /// It spreads along the edge as it ages, and is gone at the end.
    #[test]
    fn an_entry_spreads_then_fades_out() {
        let early = Light::Entry {
            along: 0.25,
            age: at(0),
        };
        let later = Light::Entry {
            along: 0.25,
            age: at(300),
        };
        assert!(
            intensity(later, 0.6) > intensity(early, 0.6),
            "reaches further"
        );
        assert!(intensity(later, 0.25) < intensity(early, 0.25), "and dims");
        let over = Light::Entry {
            along: 0.25,
            age: ENTRY_DURATION,
        };
        assert_eq!(intensity(over, 0.25), 0.0);
    }

    /// The harder the push, the more of the edge is lit, until all of it is.
    #[test]
    fn a_push_fills_the_edge_as_the_force_grows() {
        let lit = |level| {
            (0..=100)
                .filter(|step| {
                    let light = Light::Push { along: 0.2, level };
                    intensity(light, *step as f32 / 100.0) > 0.05
                })
                .count()
        };
        let (soft, firm, full) = (lit(0.2), lit(0.6), lit(1.0));
        assert!(soft < firm && firm < full, "{soft} {firm} {full}");
        assert_eq!(full, 101, "full force lights every point, whatever `along`");
    }

    /// A push is brighter the harder it is, at the place it is made.
    #[test]
    fn a_push_is_brighter_with_more_force() {
        let at_level = |level| intensity(Light::Push { along: 0.5, level }, 0.5);
        assert!(at_level(0.1) < at_level(0.5));
        assert!(at_level(0.5) < at_level(1.0));
        assert_eq!(at_level(0.0), 0.25 * 1.0, "a touch is a faint glow");
    }

    /// Letting go takes the light back, smoothly, to nothing.
    #[test]
    fn a_push_that_ended_fades_to_nothing() {
        let fading = |millis| {
            intensity(
                Light::Fading {
                    along: 0.5,
                    level: 0.8,
                    since: at(millis),
                },
                0.5,
            )
        };
        assert!(fading(0) > fading(100));
        assert!(fading(100) > fading(300));
        assert_eq!(fading(RELEASE_DURATION.as_millis() as u64), 0.0);
    }

    /// Whatever it is given, the brightness stays in range and is a number.
    #[test]
    fn intensity_is_always_a_valid_brightness() {
        for along in [-3.0, 0.0, 0.5, 1.0, 8.0, f32::NAN, f32::INFINITY] {
            for level in [-1.0, 0.0, 0.5, 1.0, 40.0, f32::NAN] {
                for point in [-1.0, 0.0, 0.3, 1.0, 9.0] {
                    let value = intensity(Light::Push { along, level }, point);
                    assert!(
                        value.is_finite() && (0.0..=1.0).contains(&value),
                        "{along} {level} {point} gave {value}"
                    );
                }
            }
        }
    }

    /// The chosen colour is kept; the rainbow turns with time and place.
    #[test]
    fn colour_is_the_chosen_one_unless_the_rainbow_is_on() {
        let chosen = (0.2, 0.4, 0.6);
        assert_eq!(colour(chosen, false, 12.0, 0.7), chosen);
        let a = colour(chosen, true, 0.0, 0.0);
        let b = colour(chosen, true, 1.0, 0.0);
        let c = colour(chosen, true, 0.0, 0.6);
        assert_ne!(a, b, "turns with time");
        assert_ne!(a, c, "and differs along the edge");
        for (r, g, bl) in [a, b, c] {
            assert!(
                (0.0..=1.0).contains(&r) && (0.0..=1.0).contains(&g) && (0.0..=1.0).contains(&bl)
            );
        }
    }

    /// Hue sweeps the primaries in order.
    #[test]
    fn hue_visits_the_primaries() {
        assert_eq!(hsv(0.0, 1.0, 1.0), (1.0, 0.0, 0.0));
        assert_eq!(hsv(1.0 / 3.0, 1.0, 1.0).1, 1.0);
        assert!(hsv(2.0 / 3.0, 1.0, 1.0).2 > 0.99);
    }

    /// A colour written and read back is the same colour, and what is shown
    /// for an empty value is the fallback, never black.
    #[test]
    fn colour_settings_round_trip_and_fall_back_to_the_accent() {
        for rgb in [
            (0, 0, 0),
            (255, 255, 255),
            (113, 131, 85),
            (1, 2, 3),
            (254, 0, 128),
        ] {
            let text = to_hex(rgb.0, rgb.1, rgb.2);
            assert_eq!(setting_colour(&text, (9, 9, 9)), rgb, "{text}");
        }
        assert_eq!(to_hex(255, 136, 0), "#ff8800");
        let accent = (113, 131, 85);
        for not_a_colour in ["", "red", "#ff88", "#zzzzzz", "ff8800"] {
            assert_eq!(
                setting_colour(not_a_colour, accent),
                accent,
                "{not_a_colour:?}"
            );
        }
    }

    /// Only a six digit hex colour is accepted.
    #[test]
    fn hex_colours_are_parsed_strictly() {
        assert_eq!(parse_hex("#ff0000"), Some((1.0, 0.0, 0.0)));
        assert_eq!(parse_hex("#00ff00"), Some((0.0, 1.0, 0.0)));
        assert_eq!(parse_hex("#718355").map(|c| c.0 > 0.4), Some(true));
        for bad in ["", "ff0000", "#ff00", "#gg0000", "#ff00000", "#ff0000 "] {
            assert_eq!(parse_hex(bad), None, "{bad:?}");
        }
    }
}
