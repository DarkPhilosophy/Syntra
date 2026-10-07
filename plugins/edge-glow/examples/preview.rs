//! Renders the glow to a PNG so its look can be judged without opening a
//! window: `preview <out.png>`.
//!
//! Draws a dark backdrop with a strip lit on each edge, the way they would sit
//! on a screen, using exactly the code the plugin paints with.

use std::{fs::File, io::BufWriter, time::Duration};

use syntra_plugin_api::Edge;
use syntra_plugin_edge_glow::{
    glow::Light,
    paint::{self, Paint},
};

const WIDTH: u32 = 960;
const HEIGHT: u32 = 540;
const STRIP: u32 = 72;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "glow.png".into());
    // A dim desktop-like backdrop, so transparency over something is visible.
    let mut canvas = vec![[0.09_f32, 0.10, 0.12]; (WIDTH * HEIGHT) as usize];

    let edges = [
        (
            Edge::Left,
            Light::Entry {
                along: 0.3,
                age: Duration::from_millis(40),
            },
            false,
        ),
        (
            Edge::Right,
            Light::Push {
                along: 0.6,
                level: 0.8,
            },
            false,
        ),
        (
            Edge::Top,
            Light::Entry {
                along: 0.5,
                age: Duration::from_millis(250),
            },
            true,
        ),
        (
            Edge::Bottom,
            Light::Push {
                along: 0.25,
                level: 1.0,
            },
            false,
        ),
    ];
    for (edge, light, rainbow) in edges {
        let paint = Paint {
            edge,
            light,
            colour: (0.44, 0.78, 0.33),
            rainbow,
            clock: 0.4,
        };
        let (w, h, x0, y0) = match edge {
            Edge::Left => (STRIP, HEIGHT, 0, 0),
            Edge::Right => (STRIP, HEIGHT, WIDTH - STRIP, 0),
            Edge::Top => (WIDTH, STRIP, 0, 0),
            Edge::Bottom => (WIDTH, STRIP, 0, HEIGHT - STRIP),
        };
        for y in 0..h {
            for x in 0..w {
                let [r, g, b, a] = paint::pixel(&paint, x, y, w, h);
                let under = &mut canvas[((y0 + y) * WIDTH + x0 + x) as usize];
                // Premultiplied "over", as a compositor does it.
                under[0] = r + under[0] * (1.0 - a);
                under[1] = g + under[1] * (1.0 - a);
                under[2] = b + under[2] * (1.0 - a);
            }
        }
    }

    let mut bytes = Vec::with_capacity(canvas.len() * 3);
    for pixel in &canvas {
        for channel in pixel {
            bytes.push((channel.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        }
    }
    let file = File::create(&path).expect("create output");
    let mut encoder = png::Encoder::new(BufWriter::new(file), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("png header")
        .write_image_data(&bytes)
        .expect("png data");
    println!("wrote {path}");
}
