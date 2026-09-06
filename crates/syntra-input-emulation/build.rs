//! Resolves which emulation backends are buildable on the target platform.
//!
//! Features are intersected with the platform and re-emitted as `cfg` flags
//! so backend modules gate on a single condition instead of repeating the
//! feature-and-platform test.

fn desktop_unix_target(family: &str, os: &str) -> bool {
    family == "unix" && os != "macos" && os != "android"
}

fn main() {
    let family = std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let desktop_unix = desktop_unix_target(&family, &os);
    let libei = desktop_unix && cfg!(feature = "libei");
    let x11 = desktop_unix && cfg!(feature = "x11");
    let wlroots = desktop_unix && cfg!(feature = "wlroots");
    let rdp = desktop_unix && cfg!(feature = "remote_desktop_portal");

    println!("cargo::rustc-check-cfg=cfg(wlroots)");
    println!("cargo::rustc-check-cfg=cfg(libei)");
    println!("cargo::rustc-check-cfg=cfg(x11)");
    println!("cargo::rustc-check-cfg=cfg(rdp)");

    if libei {
        println!("cargo::rustc-cfg=libei");
    }
    if x11 {
        println!("cargo::rustc-cfg=x11");
    }
    if wlroots {
        println!("cargo::rustc-cfg=wlroots");
    }
    if rdp {
        println!("cargo::rustc-cfg=rdp");
    }
}

#[cfg(test)]
mod tests {
    use super::desktop_unix_target;

    #[test]
    fn android_is_not_a_desktop_unix_target() {
        assert!(!desktop_unix_target("unix", "android"));
    }

    #[test]
    fn linux_remains_a_desktop_unix_target() {
        assert!(desktop_unix_target("unix", "linux"));
    }
}
