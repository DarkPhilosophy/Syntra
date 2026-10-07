//! The edge glow plugin: a glow along the screen edge a pointer entered by.
//!
//! The glow maths, the plugin's state, the daemon protocol and the choice of
//! renderer have no platform dependency and build everywhere. The windows and
//! the graphics that draw into them are per platform; Linux has them today.

pub mod glow;
pub mod paint;
pub mod protocol;
pub mod render;
pub mod state;

#[cfg(target_os = "linux")]
pub mod surface;
#[cfg(target_os = "linux")]
pub mod trace;
#[cfg(target_os = "linux")]
pub mod x11;
