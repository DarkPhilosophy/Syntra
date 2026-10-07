//! Prints how the glow looks in numbers, to judge it without a window.
//!
//! `cargo run -p syntra-plugin-edge-glow --example curve`

use std::time::Duration;

use syntra_plugin_edge_glow::glow::{Light, depth_profile, intensity};

fn main() {
    println!("brightness across the strip, by distance in pixels from the screen edge");
    println!(
        "{:>6} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "width", "0px", "4px", "10px", "20px", "40px", "80px"
    );
    for width in [16_u32, 56, 100, 160] {
        let at = |px: u32| depth_profile(px as f32 / width as f32);
        println!(
            "{width:>6} {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7.2}",
            at(0),
            at(4),
            at(10),
            at(20),
            at(40),
            at(80)
        );
    }
    println!();
    println!("share of a 2160 px edge lit by an entry (>10%), and its peak, over time");
    println!(
        "{:>8} {:>10} {:>10} {:>6}",
        "age_ms", "lit_px", "of_edge", "peak"
    );
    for ms in [0_u64, 50, 150, 300, 500, 700, 850] {
        let light = Light::Entry {
            along: 0.3,
            age: Duration::from_millis(ms),
        };
        let samples = 2160;
        let lit = (0..samples)
            .filter(|i| intensity(light, *i as f32 / samples as f32) > 0.10)
            .count();
        let peak = (0..samples)
            .map(|i| intensity(light, i as f32 / samples as f32))
            .fold(0.0, f32::max);
        println!(
            "{ms:>8} {lit:>10} {:>9.0}% {peak:>6.2}",
            lit as f32 / samples as f32 * 100.0
        );
    }
}
