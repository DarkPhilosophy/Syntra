//! The glow as pixels.
//!
//! One function says what colour every pixel of an edge strip is. The CPU
//! renderer calls it directly; the GPU shader in [`FRAGMENT_SHADER`] is the
//! same arithmetic written in GLSL. Keeping the formula in one place is what
//! lets a test on the CPU say something true about what the GPU draws.

use crate::glow::{self, Light, Rgb};
use syntra_plugin_api::Edge;

/// One pixel as premultiplied RGBA, each channel `0.0` to `1.0`.
///
/// Premultiplied because that is what a compositor expects of a window with
/// per-pixel transparency; straight alpha would show a dark fringe round the
/// glow where colour is mixed with black.
pub type Pixel = [f32; 4];

/// The transparent pixel.
pub const CLEAR: Pixel = [0.0; 4];

/// What to draw on one strip: where it lies, how long it is, and the light.
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    /// The screen edge the strip lies along.
    pub edge: Edge,
    /// The light on it.
    pub light: Light,
    /// The colour, before any rainbow shift.
    pub colour: Rgb,
    /// Whether to turn through hues instead of using `colour`.
    pub rainbow: bool,
    /// Seconds since the plugin started, which drives the rainbow.
    pub clock: f32,
}

/// The colour of the pixel at `(x, y)` of a `width` by `height` strip.
///
/// A strip for the left or right edge is `width` pixels wide and as tall as
/// the screen; for the top or bottom it is as wide as the screen and `height`
/// tall. `depth` is how far a pixel is from the screen edge, and `along` how
/// far along the edge it is.
pub fn pixel(paint: &Paint, x: u32, y: u32, width: u32, height: u32) -> Pixel {
    if width == 0 || height == 0 {
        return CLEAR;
    }
    // Pixel centres, so a one pixel strip is not sampled at its very corner.
    let fx = (x as f32 + 0.5) / width as f32;
    let fy = (y as f32 + 0.5) / height as f32;
    let (depth, along) = match paint.edge {
        Edge::Left => (fx, fy),
        Edge::Right => (1.0 - fx, fy),
        Edge::Top => (fy, fx),
        Edge::Bottom => (1.0 - fy, fx),
    };
    let strength = glow::intensity(paint.light, along) * glow::depth_profile(depth);
    if strength <= 0.0 {
        return CLEAR;
    }
    let (red, green, blue) = glow::colour(paint.colour, paint.rainbow, paint.clock, along);
    [red * strength, green * strength, blue * strength, strength]
}

/// Packs a pixel into the bytes of an `Argb8888` Wayland buffer, which on a
/// little-endian machine is blue, green, red, alpha in memory.
pub fn to_argb8888(pixel: Pixel) -> [u8; 4] {
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    [
        byte(pixel[2]),
        byte(pixel[1]),
        byte(pixel[0]),
        byte(pixel[3]),
    ]
}

/// Fills `buffer` with the strip, row by row, as `Argb8888` bytes.
///
/// `buffer` must hold `width * height * 4` bytes; a shorter one is left
/// untouched rather than written past its end.
pub fn fill_argb8888(paint: &Paint, width: u32, height: u32, buffer: &mut [u8]) {
    let needed = width as usize * height as usize * 4;
    if buffer.len() < needed {
        return;
    }
    for y in 0..height {
        for x in 0..width {
            let offset = (y as usize * width as usize + x as usize) * 4;
            buffer[offset..offset + 4]
                .copy_from_slice(&to_argb8888(pixel(paint, x, y, width, height)));
        }
    }
}

/// The fragment shader as a template. `{NAME}` markers stand for the glow's
/// constants and are filled in by [`fragment_shader`]; it must stay the same
/// arithmetic as [`pixel`] and the functions in [`crate::glow`] it calls.
const FRAGMENT_TEMPLATE: &str = r#"
#ifdef GL_ES
precision highp float;
#endif
varying vec2 v_uv;          // 0..1 across the strip, origin top-left
uniform int u_edge;         // 0 left, 1 right, 2 top, 3 bottom
uniform int u_kind;         // 0 entry, 1 push, 2 fading
uniform float u_along;      // where on the edge the light is centred
uniform float u_level;      // push force, 0..1
uniform float u_progress;   // entry or fade progress, 0..1
uniform vec3 u_colour;
uniform int u_rainbow;
uniform float u_clock;

const float ENTRY_START = {ENTRY_START};
const float ENTRY_SPREAD = {ENTRY_SPREAD};
const float PUSH_HALF = {PUSH_HALF};
const float PUSH_FLOOR = {PUSH_FLOOR};
const float DEPTH_CORE = {DEPTH_CORE};
const float DEPTH_HAZE = {DEPTH_HAZE};
const float CORE_SHARE = {CORE_SHARE};

float unit(float v) { return clamp(v, 0.0, 1.0); }

float falloff(float point, float centre, float half_width) {
    float d = abs(point - centre);
    if (half_width <= 0.0 || d >= half_width) return 0.0;
    float t = 1.0 - d / half_width;
    return t * t * (3.0 - 2.0 * t);
}

float depth_profile(float depth) {
    depth = unit(depth);
    float core = exp(-depth * DEPTH_CORE);
    float haze = exp(-depth * DEPTH_HAZE);
    float raw = CORE_SHARE * core + (1.0 - CORE_SHARE) * haze;
    float at_border = CORE_SHARE * exp(-DEPTH_CORE) + (1.0 - CORE_SHARE) * exp(-DEPTH_HAZE);
    return clamp((raw - at_border) / (1.0 - at_border), 0.0, 1.0);
}

vec3 hsv(float hue, float sat, float val) {
    float h = fract(hue) * 6.0;
    float sector = floor(h);
    float f = h - sector;
    float p = val * (1.0 - sat);
    float q = val * (1.0 - sat * f);
    float t = val * (1.0 - sat * (1.0 - f));
    int s = int(mod(sector, 6.0));
    if (s == 0) return vec3(val, t, p);
    if (s == 1) return vec3(q, val, p);
    if (s == 2) return vec3(p, val, t);
    if (s == 3) return vec3(p, q, val);
    if (s == 4) return vec3(t, p, val);
    return vec3(val, p, q);
}

void main() {
    float depth;
    float along;
    if (u_edge == 0)      { depth = v_uv.x;       along = v_uv.y; }
    else if (u_edge == 1) { depth = 1.0 - v_uv.x; along = v_uv.y; }
    else if (u_edge == 2) { depth = v_uv.y;       along = v_uv.x; }
    else                  { depth = 1.0 - v_uv.y; along = v_uv.x; }

    float spread;
    float scale;
    if (u_kind == 0) {
        spread = ENTRY_START + u_progress * ENTRY_SPREAD;
        scale = (1.0 - u_progress) * (1.0 - u_progress);
    } else if (u_kind == 1) {
        spread = PUSH_HALF;
        scale = PUSH_FLOOR + (1.0 - PUSH_FLOOR) * unit(u_level);
    } else {
        spread = PUSH_HALF;
        scale = (PUSH_FLOOR + (1.0 - PUSH_FLOOR) * unit(u_level)) * (1.0 - u_progress);
    }
    float strength = falloff(along, u_along, spread) * scale * depth_profile(depth);
    if (strength <= 0.0) discard;

    vec3 colour = u_colour;
    if (u_rainbow != 0) {
        colour = hsv(u_clock * 0.35 + clamp(along, 0.0, 1.0) * 0.5, 0.85, 1.0);
    }
    gl_FragColor = vec4(colour * strength, strength);
}
"#;

/// The fragment shader, with the glow's constants filled in from
/// [`crate::glow`] so the GPU and the CPU path cannot be tuned apart.
pub fn fragment_shader() -> String {
    use crate::glow::{
        CORE_SHARE, DEPTH_CORE, DEPTH_HAZE, ENTRY_SPREAD, ENTRY_START_HALF_WIDTH, PUSH_FLOOR,
        PUSH_HALF_WIDTH,
    };
    let mut source = FRAGMENT_TEMPLATE.to_owned();
    for (name, value) in [
        ("ENTRY_START", ENTRY_START_HALF_WIDTH),
        ("ENTRY_SPREAD", ENTRY_SPREAD),
        ("PUSH_HALF", PUSH_HALF_WIDTH),
        ("PUSH_FLOOR", PUSH_FLOOR),
        ("DEPTH_CORE", DEPTH_CORE),
        ("DEPTH_HAZE", DEPTH_HAZE),
        ("CORE_SHARE", CORE_SHARE),
    ] {
        // `{:?}` always writes a decimal point, which GLSL needs for a float.
        source = source.replace(&format!("{{{name}}}"), &format!("{value:?}"));
    }
    source
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glow::ENTRY_DURATION;
    use std::time::Duration;

    fn entry(edge: Edge, along: f32, age_ms: u64) -> Paint {
        Paint {
            edge,
            light: Light::Entry {
                along,
                age: Duration::from_millis(age_ms),
            },
            colour: (0.2, 0.8, 0.4),
            rainbow: false,
            clock: 0.0,
        }
    }

    /// The pixel nearest the screen edge, at the entry point, is the brightest
    /// of the strip; the one at the inner border is empty.
    #[test]
    fn the_glow_is_brightest_at_the_screen_edge_and_dies_inward() {
        let paint = entry(Edge::Left, 0.5, 0);
        let (w, h) = (56, 400);
        let at_edge = pixel(&paint, 0, h / 2, w, h)[3];
        let mid = pixel(&paint, w / 2, h / 2, w, h)[3];
        let inner = pixel(&paint, w - 1, h / 2, w, h)[3];
        assert!(at_edge > mid, "brighter at the edge than halfway in");
        assert!(mid > inner, "and brighter halfway in than at the border");
        assert!(inner < 0.02, "essentially nothing at the inner border");
    }

    /// Each edge is lit from its own side: the bright side of a right strip
    /// is its right-hand pixel column, of a bottom strip its last row.
    #[test]
    fn every_edge_is_brightest_on_the_side_that_touches_the_screen_edge() {
        let (w, h) = (300, 300);
        let probe = |edge: Edge, near: (u32, u32), far: (u32, u32)| {
            let paint = entry(edge, 0.5, 0);
            let near = pixel(&paint, near.0, near.1, w, h)[3];
            let far = pixel(&paint, far.0, far.1, w, h)[3];
            assert!(near > far, "{edge:?}: {near} should exceed {far}");
        };
        probe(Edge::Left, (0, 150), (299, 150));
        probe(Edge::Right, (299, 150), (0, 150));
        probe(Edge::Top, (150, 0), (150, 299));
        probe(Edge::Bottom, (150, 299), (150, 0));
    }

    /// An entry lights where the pointer came in, not along the whole edge.
    #[test]
    fn an_entry_lights_only_near_the_entry_point() {
        let paint = entry(Edge::Left, 0.2, 0);
        let (w, h) = (56, 1000);
        let near = pixel(&paint, 0, 200, w, h)[3];
        let far = pixel(&paint, 0, 900, w, h)[3];
        assert!(near > 0.9);
        assert_eq!(far, 0.0);
    }

    /// A finished glow is entirely transparent, so a window that has not been
    /// hidden yet shows nothing rather than a faint residue.
    #[test]
    fn a_finished_entry_is_fully_transparent() {
        let paint = Paint {
            light: Light::Entry {
                along: 0.5,
                age: ENTRY_DURATION,
            },
            ..entry(Edge::Left, 0.5, 0)
        };
        for y in (0..400).step_by(37) {
            assert_eq!(pixel(&paint, 0, y, 56, 400), CLEAR);
        }
    }

    /// Premultiplied: no colour channel may exceed alpha, or the compositor
    /// would brighten whatever is behind the window.
    #[test]
    fn colour_never_exceeds_alpha() {
        let paint = Paint {
            rainbow: true,
            clock: 1.7,
            ..entry(Edge::Top, 0.5, 120)
        };
        for x in (0..800).step_by(41) {
            for y in 0..56 {
                let [r, g, b, a] = pixel(&paint, x, y, 800, 56);
                assert!(r <= a + 1e-5 && g <= a + 1e-5 && b <= a + 1e-5, "({x},{y})");
                assert!((0.0..=1.0).contains(&a));
            }
        }
    }

    /// A strip of zero size, or a buffer too small for it, must not panic or
    /// write out of bounds; the plugin runs for as long as the session does.
    #[test]
    fn degenerate_sizes_are_harmless() {
        let paint = entry(Edge::Left, 0.5, 0);
        assert_eq!(pixel(&paint, 0, 0, 0, 10), CLEAR);
        assert_eq!(pixel(&paint, 0, 0, 10, 0), CLEAR);
        let mut small = [7_u8; 8];
        fill_argb8888(&paint, 10, 10, &mut small);
        assert_eq!(small, [7; 8], "a too-small buffer is left alone");
    }

    /// The bytes are blue, green, red, alpha, as an Argb8888 buffer wants.
    #[test]
    fn argb8888_byte_order_is_bgra() {
        assert_eq!(to_argb8888([1.0, 0.5, 0.0, 1.0]), [0, 128, 255, 255]);
    }

    /// The shader is built from the same constants the CPU path uses, so the
    /// two cannot drift apart. What can still differ is the arithmetic, so
    /// the source is checked for being generated from those values and not
    /// from literals typed twice.
    #[test]
    fn the_shader_is_built_from_the_cpu_constants() {
        let shader = fragment_shader();
        for (name, value) in [
            ("ENTRY_START", crate::glow::ENTRY_START_HALF_WIDTH),
            ("ENTRY_SPREAD", crate::glow::ENTRY_SPREAD),
            ("PUSH_HALF", crate::glow::PUSH_HALF_WIDTH),
            ("PUSH_FLOOR", crate::glow::PUSH_FLOOR),
            ("DEPTH_CORE", crate::glow::DEPTH_CORE),
            ("DEPTH_HAZE", crate::glow::DEPTH_HAZE),
            ("CORE_SHARE", crate::glow::CORE_SHARE),
        ] {
            let declared = format!("const float {name} = {value:?};");
            assert!(
                shader.contains(&declared),
                "the shader does not declare `{declared}`"
            );
        }
        // None of them may also appear as a bare literal in the arithmetic.
        for stale in ["7.0)", "exp(-2.2)", "SEED", "* 1.2;"] {
            assert!(
                !shader.contains(stale),
                "the shader still contains the old literal `{stale}`"
            );
        }
    }
}
