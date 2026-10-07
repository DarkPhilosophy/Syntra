//! What the glow looks like, apart from how it reaches the screen.
//!
//! Two things light an edge. A pointer that comes in from another device
//! lights it from the place it entered, spreading along the edge and fading.
//! A pointer being pushed out lights it in proportion to the push, from where
//! the pointer is, until it fills the edge at the moment it passes on. Both
//! reduce to one question a renderer asks every frame: how bright is each
//! point of the edge, how far in from the edge does the light reach, and what
//! colour is it. Nothing here draws, so it is the same on every desktop and
//! can be tested without one.

use std::time::Duration;

/// How long an entry glow takes to spread and fade away.
pub const ENTRY_DURATION: Duration = Duration::from_millis(900);
/// How long a glow built by pressure takes to fade once the push is over.
pub const RELEASE_DURATION: Duration = Duration::from_millis(350);
/// Share of the edge an entry covers at its start, either side of its centre.
pub(crate) const ENTRY_START_HALF_WIDTH: f32 = 0.10;
/// How much further, as a share of the edge, an entry spreads by its end.
pub(crate) const ENTRY_SPREAD: f32 = 1.2;
/// Share of the edge a push covers either side of where the pointer is, kept
/// the same at every strength.
pub(crate) const PUSH_HALF_WIDTH: f32 = 0.28;
/// The least brightness a push has, however light: below this it is not seen.
pub(crate) const PUSH_FLOOR: f32 = 0.45;
/// How quickly the bright core of the strip dies away from the screen edge, as
/// an exponent over the strip's depth. Small enough that the width the user
/// chose is the thickness they see: with 7 only a seventh of it was visible.
pub(crate) const DEPTH_CORE: f32 = 3.2;
/// How quickly the faint haze dies away, same scale.
pub(crate) const DEPTH_HAZE: f32 = 1.2;
/// Share of the brightness at the screen edge that comes from the core.
pub(crate) const CORE_SHARE: f32 = 0.72;

/// The kind of light on one edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Light {
    /// A pointer arrived at `along`; `age` is how long ago.
    Entry {
        /// Where along the edge it came in, `0.0` to `1.0`.
        along: f32,
        /// How long ago.
        age: Duration,
    },
    /// A pointer is being pushed out at `along` with `level` of the force
    /// needed to pass on, from `0.0` to `1.0`.
    Push {
        /// Where along the edge the pointer is.
        along: f32,
        /// How close the push is to crossing.
        level: f32,
    },
    /// A push that has just ended, fading from `level`.
    Fading {
        /// Where along the edge the pointer was.
        along: f32,
        /// How strong the push was when it ended.
        level: f32,
        /// How long ago it ended.
        since: Duration,
    },
}

impl Light {
    /// Whether this light is still visible `now`, so a renderer can stop
    /// drawing, and stop waking, once everything has faded.
    pub fn is_over(&self) -> bool {
        match *self {
            Light::Entry { age, .. } => age >= ENTRY_DURATION,
            Light::Fading { since, .. } => since >= RELEASE_DURATION,
            Light::Push { .. } => false,
        }
    }
}

/// Brightness, from `0.0` to `1.0`, at `point` along an edge (`0.0` to `1.0`).
///
/// An entry opens from its centre and fades; a push widens with its level
/// until the whole edge is lit; a push that ended fades back.
pub fn intensity(light: Light, point: f32) -> f32 {
    let point = unit(point);
    match light {
        Light::Entry { along, age } => {
            let progress = (age.as_secs_f32() / ENTRY_DURATION.as_secs_f32()).clamp(0.0, 1.0);
            if progress >= 1.0 {
                return 0.0;
            }
            // Spreads to cover the edge, and dims as it does.
            let half = ENTRY_START_HALF_WIDTH + progress * ENTRY_SPREAD;
            let fade = (1.0 - progress).powi(2);
            falloff(point, along, half) * fade
        }
        Light::Push { along, level } => {
            let level = unit(level);
            // The spread along the edge is fixed and only the brightness
            // follows the strength of the push. Widening with the force made a
            // light push a dot and a hard one the whole edge, which read as the
            // glow changing size at random.
            falloff(point, along, PUSH_HALF_WIDTH) * (PUSH_FLOOR + (1.0 - PUSH_FLOOR) * level)
        }
        Light::Fading {
            along,
            level,
            since,
        } => {
            let progress = (since.as_secs_f32() / RELEASE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
            let level = unit(level);
            falloff(point, along, PUSH_HALF_WIDTH)
                * (PUSH_FLOOR + (1.0 - PUSH_FLOOR) * level)
                * (1.0 - progress)
        }
    }
}

/// How much of the light is left at `depth` (`0.0` at the screen edge, `1.0`
/// at the far side of the strip).
///
/// A real glow is brightest at its source and dies away quickly, then lingers
/// as a faint haze. A straight line to black reads as a painted gradient; this
/// is a bright core with a soft exponential tail, which is what makes it look
/// like light spilling in from the edge.
pub fn depth_profile(depth: f32) -> f32 {
    let depth = unit(depth);
    // The core and the haze, summed so neither alone sets the look.
    let core = (-depth * DEPTH_CORE).exp();
    let haze = (-depth * DEPTH_HAZE).exp();
    // Zero exactly at the inner border, so the strip has no visible edge.
    let raw = CORE_SHARE * core + (1.0 - CORE_SHARE) * haze;
    let at_border = CORE_SHARE * (-DEPTH_CORE).exp() + (1.0 - CORE_SHARE) * (-DEPTH_HAZE).exp();
    ((raw - at_border) / (1.0 - at_border)).clamp(0.0, 1.0)
}

/// A value held to `0.0..=1.0`, where anything that is not a number is `0.0`.
/// `NaN.clamp` stays `NaN`, and one such value would turn every pixel of the
/// glow into `NaN` for as long as it lasted.
fn unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// `1.0` at `centre`, down to `0.0` `half` away, smooth in between.
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

/// A colour as red, green and blue from `0.0` to `1.0`.
pub type Rgb = (f32, f32, f32);

/// The colour of the glow now. `base` is the chosen colour; with `rainbow`
/// the hue turns with `clock` (seconds) instead, and `along` shifts it so the
/// glow is not one flat colour end to end.
pub fn colour(base: Rgb, rainbow: bool, clock: f32, along: f32) -> Rgb {
    if !rainbow {
        return base;
    }
    let hue = (clock * 0.35 + along.clamp(0.0, 1.0) * 0.5).rem_euclid(1.0);
    hsv(hue, 0.85, 1.0)
}

/// Hue in `0.0..1.0` to red, green and blue, at the given saturation and value.
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
/// that is empty or malformed falls back to the default colour.
pub fn parse_hex(value: &str) -> Option<Rgb> {
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
            age: at(400),
        };
        assert!(
            intensity(later, 0.6) > intensity(early, 0.6),
            "the light reaches further along the edge as it ages"
        );
        assert!(
            intensity(later, 0.25) < intensity(early, 0.25),
            "and is dimmer where it started"
        );
        let over = Light::Entry {
            along: 0.25,
            age: ENTRY_DURATION,
        };
        assert_eq!(intensity(over, 0.25), 0.0);
        assert!(over.is_over(), "a finished entry lets the renderer sleep");
    }

    /// A push keeps the same spread whatever its strength; only the brightness
    /// follows. Spread that grew with the force made a light push a dot and a
    /// hard one the whole edge, which read as the glow changing size by itself.
    #[test]
    fn a_push_keeps_its_spread_and_only_its_brightness_follows_the_force() {
        let lit_share = |level: f32| {
            let light = Light::Push { along: 0.5, level };
            // Any light at all is the spread itself; a brightness threshold
            // would also move with the force, since a dimmer tail crosses it
            // sooner.
            (0..1000)
                .filter(|i| intensity(light, *i as f32 / 1000.0) > 0.0)
                .count()
        };
        assert_eq!(
            lit_share(0.1),
            lit_share(1.0),
            "the same spread at any force"
        );
        let weak = Light::Push {
            along: 0.5,
            level: 0.1,
        };
        let full = Light::Push {
            along: 0.5,
            level: 1.0,
        };
        assert!(
            intensity(full, 0.5) > intensity(weak, 0.5),
            "harder is brighter"
        );
        assert!(!full.is_over(), "a push lasts as long as it continues");
    }

    /// Even the lightest push must be seen: below a floor it was a faint
    /// smudge that people reported as the glow not working at all.
    #[test]
    fn the_lightest_push_is_still_clearly_visible() {
        let barely = Light::Push {
            along: 0.5,
            level: 0.01,
        };
        assert!(
            intensity(barely, 0.5) >= PUSH_FLOOR * 0.99,
            "a push of almost no force is dimmer than the floor"
        );
    }

    /// The width the user sets must be the thickness they see. With the old
    /// steep profile only about a seventh of the strip was lit, so changing
    /// the width looked like it did nothing.
    #[test]
    fn the_visible_thickness_follows_the_width_setting() {
        // How much of the strip is brighter than a fifth of the maximum.
        let visible = (0..1000)
            .filter(|i| depth_profile(*i as f32 / 1000.0) > 0.20)
            .count() as f32
            / 1000.0;
        assert!(
            visible > 0.30,
            "only {:.0}% of the strip is clearly lit, so the width setting is not seen",
            visible * 100.0
        );
    }

    /// A push that ended fades away rather than switching off.
    #[test]
    fn a_released_push_fades_to_nothing() {
        let just_ended = Light::Fading {
            along: 0.5,
            level: 0.8,
            since: at(0),
        };
        let halfway = Light::Fading {
            along: 0.5,
            level: 0.8,
            since: at(175),
        };
        let gone = Light::Fading {
            along: 0.5,
            level: 0.8,
            since: RELEASE_DURATION,
        };
        assert!(intensity(halfway, 0.5) < intensity(just_ended, 0.5));
        assert_eq!(intensity(gone, 0.5), 0.0);
        assert!(gone.is_over());
    }

    /// Values a malformed event could carry must never reach the screen as
    /// `NaN`, which would blank the whole strip for as long as it lasted.
    #[test]
    fn nonsense_input_never_produces_nan() {
        for along in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -5.0, 9.0] {
            for light in [
                Light::Entry {
                    along,
                    age: at(100),
                },
                Light::Push {
                    along,
                    level: along,
                },
                Light::Fading {
                    along,
                    level: along,
                    since: at(50),
                },
            ] {
                for point in [0.0, 0.5, 1.0, f32::NAN] {
                    let value = intensity(light, point);
                    assert!(value.is_finite() && (0.0..=1.0).contains(&value));
                }
            }
        }
    }

    /// The light is brightest at the screen edge and dies away inward,
    /// without a visible border where the strip ends.
    #[test]
    fn light_is_brightest_at_the_edge_and_gone_at_the_inner_border() {
        assert!((depth_profile(0.0) - 1.0).abs() < 1e-4, "full at the edge");
        assert_eq!(depth_profile(1.0), 0.0, "nothing left at the border");
        let mut last = depth_profile(0.0);
        for step in 1..=100 {
            let now = depth_profile(step as f32 / 100.0);
            assert!(now <= last + 1e-6, "never brighter further in");
            last = now;
        }
    }

    /// What makes it a glow rather than a gradient: most of the light is
    /// close to the edge, with a long faint tail, not an even slope.
    #[test]
    fn the_light_falls_off_faster_than_a_straight_line() {
        let linear_midpoint = 0.5;
        assert!(
            depth_profile(0.5) < linear_midpoint,
            "halfway in it is dimmer than a straight slope would be"
        );
        assert!(
            depth_profile(0.5) > 0.0,
            "yet a faint haze is still there, not a hard cut-off"
        );
    }

    #[test]
    fn colours_parse_and_malformed_ones_do_not() {
        assert_eq!(parse_hex("#ff0000"), Some((1.0, 0.0, 0.0)));
        for bad in ["", "ff0000", "#ff00", "#gg0000", "#ff00000"] {
            assert_eq!(parse_hex(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn rainbow_moves_with_the_clock_and_a_fixed_colour_does_not() {
        let base = (0.2, 0.4, 0.6);
        assert_eq!(colour(base, false, 3.0, 0.5), base);
        assert_ne!(colour(base, true, 0.0, 0.5), colour(base, true, 1.0, 0.5));
    }
}
