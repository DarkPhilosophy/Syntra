fn desktop_unix_target(family: &str, os: &str) -> bool {
    family == "unix" && os != "macos" && os != "android"
}

fn main() {
    let family = std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let desktop_unix = desktop_unix_target(&family, &os);
    let layer_shell = desktop_unix && cfg!(feature = "layer_shell");
    let libei = desktop_unix && cfg!(feature = "libei");
    let x11 = desktop_unix && cfg!(feature = "x11");

    println!("cargo::rustc-check-cfg=cfg(layer_shell)");
    println!("cargo::rustc-check-cfg=cfg(libei)");
    println!("cargo::rustc-check-cfg=cfg(x11)");

    if layer_shell {
        println!("cargo::rustc-cfg=layer_shell");
    }
    if libei {
        println!("cargo::rustc-cfg=libei");
    }
    if x11 {
        println!("cargo::rustc-cfg=x11");
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
