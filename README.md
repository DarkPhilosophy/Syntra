# Syntra

[![CI](https://github.com/DarkPhilosophy/syntra/actions/workflows/rust.yml/badge.svg)](https://github.com/DarkPhilosophy/syntra/actions/workflows/rust.yml) [![Cachix](https://github.com/DarkPhilosophy/syntra/actions/workflows/cachix.yml/badge.svg)](https://github.com/DarkPhilosophy/syntra/actions/workflows/cachix.yml) [![Release](https://github.com/DarkPhilosophy/syntra/actions/workflows/release.yml/badge.svg)](https://github.com/DarkPhilosophy/syntra/actions/workflows/release.yml)

[![crates.io](https://img.shields.io/crates/v/syntra.svg)](https://crates.io/crates/syntra)  [![license](https://img.shields.io/crates/l/syntra.svg)](https://github.com/DarkPhilosophy/syntra/blob/main/Cargo.toml)

Syntra is a *cross-platform* mouse and keyboard sharing software similar to universal-control on Apple devices.
It allows for using multiple PCs via a single set of mouse and keyboard.
This is also known as a Software KVM switch.

Goal of this project is to be an open-source alternative to proprietary tools like [Synergy 2/3](https://symless.com/synergy), [Share Mouse](https://www.sharemouse.com/de/)
and other open source tools like [Deskflow](https://github.com/deskflow/deskflow) or [Input Leap](https://github.com/input-leap) (Synergy fork).

The desktop application uses a native [Slint](https://slint.dev/) frontend on Linux, Windows,
and macOS. Android build and lifecycle integration is present in the source tree but is not yet
a verified, supported application.

<picture>
    <source media="(prefers-color-scheme: dark)" srcset="/screenshots/dark.png?raw=true">
    <source media="(prefers-color-scheme: light)" srcset="/screenshots/light.png?raw=true">
    <img alt="Screenshot of Syntra" srcset="/screenshots/dark.png">
</picture>


## Encryption

Syntra encrypts all network traffic using the DTLS implementation provided by [WebRTC.rs](https://github.com/webrtc-rs/webrtc).
There are currently no mitigations in place for timing side-channel attacks.

## OS Support

Most current desktop environments and operating systems are fully supported, this includes
- GNOME >= 45
- KDE Plasma >= 6.1
- Most wlroots based compositors, including Sway (>= 1.8), Hyprland and Wayfire
- Windows
- MacOS


### Caveats / Known Issues

> [!Important]
> - **X11** currently only has support for input emulation, i.e. can only be used on the receiving end.
>
> - **Sway / wlroots**: Wlroots based compositors without libei support on the receiving end currently do not handle modifier events on the client side.
> This results in CTRL / SHIFT / ALT / SUPER keys not working with a sending device that is NOT using the `layer-shell` backend
>
> - **Wayfire**: If you are using [Wayfire](https://github.com/WayfireWM/wayfire), make sure to use a recent version (must be newer than October 23rd) and **add `shortcuts-inhibit` to the list of plugins in your wayfire config!**
> Otherwise input capture will not work.
>
> - **Windows**: The mouse cursor will be invisible when sending input to a Windows system if
> there is no real mouse connected to the machine.

For more detailed information about os support see [Detailed OS Support](#detailed-os-support)

### Android and iOS

The in-tree Android work currently provides a Gradle project, lifecycle/transport foundations,
and explicit capability reporting. It is not yet a verified release target: the Android UI
host does not currently establish a complete runnable Syntra session, and input capture,
input emulation, and file clipboard transfer are reported as unavailable. Do not treat an APK
produced by the build pipeline as proof of those capabilities.

The separate historical [Android/iOS proof of concept](https://github.com/rohitsangwan01/syntra-mobile)
is not the supported desktop application described by this README.

## Installation

<details>
    <summary>Arch Linux</summary>

Syntra can be installed from the [official repositories](https://archlinux.org/packages/extra/x86_64/syntra/):

```sh
pacman -S syntra
```

The prerelease version (following `main`) is available on the AUR:

```sh
paru -S syntra-git
```
</details>


<details>
    <summary>Nix (OS)</summary>

- nixpkgs: [search.nixos.org](https://search.nixos.org/packages?channel=unstable&show=syntra&from=0&size=50&sort=relevance&type=packages&query=syntra)
- flake: [README.md](./nix/README.md)
</details>

<details>
    <summary>Fedora</summary>
You can install Syntra from the [Terra Repository](https://terra.fyralabs.com).


After enabling Terra:

```sh
dnf install syntra
```
</details>

<details>
    <summary>MacOS</summary>

- Download the package for your Mac (Intel or ARM) from the releases page
- Unzip it
- Remove the quarantine with `xattr -rd com.apple.quarantine "Syntra.app"`
- Launch the app
- Use the menu bar item to open the settings window or quit Syntra. Bundled macOS builds run as a menu bar app and do not keep a Dock icon visible.
- Grant accessibility permissions in System Preferences

</details>


<details>
    <summary>Manual Installation</summary>

First make sure to [install the necessary dependencies](#installing-dependencies-for-development--compiling-from-source).

Upstream Syntra binaries are available in the [releases section](https://github.com/DarkPhilosophy/syntra/releases). Those releases may not include the Syntra changes in this working tree.
The current Windows packaging workflow creates a ZIP containing `syntra.exe`; it does not bundle additional DLLs. See [Installing Dependencies](#installing-dependencies-for-development--compiling-from-source) for platform prerequisites.

Alternatively, Syntra and its required helper executables can be compiled from source.

### Installing the desktop file, app icon, and firewall rules (optional)
```sh
# install syntra and the Linux clipboard/file-transfer helpers
sudo install -Dm755 target/release/syntra /usr/local/bin/syntra
sudo install -Dm755 target/release/syntra-plugin-gtk-clipboard /usr/local/bin/syntra-plugin-gtk-clipboard
sudo install -Dm755 target/release/syntra-plugin-fuse /usr/local/bin/syntra-plugin-fuse

# install app icon and desktop entry
sudo install -Dm644 syntra-ui/ui/assets/shell/syntra.svg /usr/local/share/icons/hicolor/scalable/apps/syntra.svg
sudo install -Dm644 io.syntra.Syntra.desktop /usr/local/share/applications/io.syntra.Syntra.desktop

# update icon cache
gtk-update-icon-cache /usr/local/share/icons/hicolor/

# when using firewalld: install firewall rule
sudo install -Dm644 firewall/syntra.xml /etc/firewalld/services/syntra.xml
# -> enable the service in firewalld settings
```

The `syntra-plugin-gtk-clipboard` binary is a separate Linux runtime helper used for
clipboard integration. It is **not** the application frontend. Linux packages must ship it
beside `syntra` until a verified non-GTK clipboard adapter replaces it; packages supporting
file transfer must also ship `syntra-plugin-fuse`.

Instead of downloading a release, build the Slint desktop application and Linux helpers with:

```sh
cargo build --release \
  -p syntra -p syntra-plugin-gtk-clipboard -p syntra-plugin-fuse
```

The default feature set selects Slint and preserves all Linux capture and emulation backends.

### Compiling and installing via cargo:
```sh
# will end up in ~/.cargo/bin
cargo install syntra
```

### Compiling and installing via nix:
```sh
# you can find the executable in result/bin/syntra
nix-build
```
### Conditional compilation

Support for unavailable platform backends is omitted automatically based on the active Rust
toolchain. Capture and emulation backends can also be selected manually with
[Cargo features](https://doc.rust-lang.org/cargo/reference/features.html).

For example, this builds Slint with only layer-shell capture and wlroots emulation:

```sh
cargo build --no-default-features --features slint-ui,layer_shell_capture,wlroots_emulation
```

See [Cargo.toml](./Cargo.toml) for the complete feature list.
</details>



## Development

### Git pre-commit hook

This repository includes a local git hooks directory `.githooks/` with a `pre-commit` script that enforces formatting, lints, and tests before allowing a commit.  It is optional to enable it, but it will prevent you from committing code with failing unit tests or that needs clippy/fmt fixes. To enable the hook locally:

1. Make the hook executable:

```sh
chmod +x .githooks/pre-commit
```

2. Point git to the hooks directory (one-time per clone):

```sh
git config core.hooksPath .githooks
```

The `pre-commit` script runs `cargo fmt --all`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

### Dependencies and compiling from source

The Slint application does not require GTK or libadwaita on Windows or macOS. Linux requires
GTK 4 only for the separately packaged `syntra-plugin-gtk-clipboard` helper.

<details>
    <summary>macOS</summary>

```sh
brew install pkg-config imagemagick
cargo install cargo-bundle
scripts/makeicns.sh
cargo bundle
```
</details>

<details>
    <summary>Ubuntu and derivatives</summary>

```sh
sudo apt install libgtk-4-dev libx11-dev libxtst-dev
```
</details>

<details>
    <summary>Arch and derivatives</summary>

```sh
sudo pacman -S gtk4 libx11 libxtst
```
</details>

<details>
    <summary>Fedora and derivatives</summary>

```sh
sudo dnf install gtk4-devel libXtst-devel libX11-devel
```
</details>
<details>
    <summary>Nix</summary>

```sh
nix-shell .
```
</details>
<details>
    <summary>Nix (flake)</summary>

```sh
nix develop
```
</details>


## Usage

<details>
    <summary>Slint frontend</summary>

The Syntra Slint frontend uses the existing daemon/service IPC boundary. The titlebar
toggle collapses the navigation to an icon rail without changing the selected page.
Presentation settings include locale, independent White/Black mode and palette, density,
interface scale, sidebar state, and local device names and images.

The localization runtime embeds `en-US`, `de-DE`, `ro-RO`, and the `en-XA` pseudo-locale.
Locale changes update the localization state and observers without requiring a process
restart. The current UI migration is still replacing remaining hard-coded visible strings,
so this is infrastructure behavior rather than a claim that every screen is fully translated.

To connect a device you want to control, add its hostname. On the remote device, authorize
the local device for incoming traffic. If it cannot be reached, ensure UDP port `4242` (or
the configured port) is open in the firewall.

The Devices page discovers nearby peers through mDNS (`_syntra._udp.local.`).
Discovery is informational: a discovered device is not automatically configured or authorized.
Refresh restarts discovery; unavailable mDNS does not prevent manually configured connections.

Manual file transfer is separate from automatic clipboard and FUSE synchronization. Files can
be sent only to connected, authenticated peers, either by clicking a peer in the transfer
picker or by using native file drag-and-drop. Before acceptance, the receiver sees metadata
only. Accepted files default to the Downloads directory, and the destination can be edited;
an existing file is never overwritten. Automatic acceptance is opt-in and defaults to
`false`.

On Linux, native file drag-and-drop in the Slint UI requires XWayland. This UI dependency does
not change the daemon's Wayland input-capture backend. Windows and macOS support is not
runtime-verified by these instructions.

The Clipboard page lists installed Flatpak applications. Granting access is an explicit
per-application action restricted to `xdg-run/syntra:ro`; it does not grant general
home-directory access. Restart the affected Flatpak application to apply its override.
</details>

<details>
    <summary>Command Line Interface</summary>

The cli interface can be accessed by passing `cli` as a commandline argument.
Use
```sh
syntra cli help
```
 to list the available commands and
```sh
syntra cli <cmd> help
```
for information on how to use a specific command.

</details>

<details>
    <summary>Daemon Mode</summary>

Syntra can be launched in daemon mode to keep it running in the background (e.g. for use in a systemd-service).

To do so, use the `daemon` subcommand:

```sh
syntra daemon
```

The frontend first attaches to an existing daemon. If none is reachable, it starts an
installed Linux user service or, without one, a temporary child daemon. Closing the
frontend stops only a temporary child it owns; an independently running service survives.
</details>

## Systemd Service

On Linux, Settings shows installation, running state, and autostart independently.
Installation writes a user unit for the current production executable and reloads systemd;
it does not implicitly enable autostart. Start, stop, uninstall, and autostart changes are
separate confirmed actions. Starting the installed service while a temporary daemon is
connected briefly interrupts sharing while the frontend reconnects. Non-Linux service
management is reported as unavailable.

In order to start syntra with a graphical session automatically,
the [systemd-service](service/syntra.service) can be used:

Copy the file to `~/.config/systemd/user/` and enable the service:

```sh
cp service/syntra.service ~/.config/systemd/user
systemctl --user daemon-reload
systemctl --user enable --now syntra.service
```
> [!Important]
> Make sure to point `ExecStart=/usr/bin/syntra daemon` to the actual `syntra` binary (in case it is not under `/usr/bin`, e.g. when installed manually.


## Configuration
To automatically load clients on startup, the file `$XDG_CONFIG_HOME/syntra/config.toml` is parsed.
`$XDG_CONFIG_HOME` defaults to `~/.config/`.

To create this file you can copy the following example config:

### Example config
> [!TIP]
> key symbols in the release bind are named according
> to their names in [input-event/src/scancode.rs#L172](input-event/src/scancode.rs#L176).
> This is bound to change

```toml
# example configuration

# configure release bind
release_bind = [ "KeyA", "KeyS", "KeyD", "KeyF" ]

# optional port (defaults to 4242)
port = 4242

# list of authorized tls certificate fingerprints that
# are accepted for incoming traffic
[authorized_fingerprints]
"bc:05:ab:7a:a4:de:88:8c:2f:92:ac:bc:b8:49:b8:24:0d:44:b3:e6:a4:ef:d7:0b:6c:69:6d:77:53:0b:14:80" = "iridium"

# define a client on the right side with host name "iridium"
[[clients]]
# position (left | right | top | bottom)
position = "right"
# hostname
hostname = "iridium"
# activate this client immediately when syntra is started
activate_on_startup = true
# optional list of (known) ip addresses
ips = ["192.168.178.156"]

# define a client on the left side with IP address 192.168.178.189
[[clients]]
position = "left"
# The hostname is optional: When no hostname is specified,
# at least one ip address needs to be specified.
hostname = "thorium"
# ips for ethernet and wifi
ips = ["192.168.178.189", "192.168.178.172"]
# optional port
port = 4242
```

Where `left` can be either `left`, `right`, `top` or `bottom`.

## Roadmap
- [x] Slint is the default and sole graphical frontend; GTK remains only in the separately packaged Linux clipboard helper.
- [x] respect xdg-config-home for config file location.
- [x] IP Address switching
- [x] Liveness tracking Automatically ungrab mouse when client unreachable
- [x] Liveness tracking: Automatically release keys, when server offline
- [x] MacOS KeyCode Translation
- [x] Libei Input Capture
- [x] MacOS Input Capture
- [x] Windows Input Capture
- [x] Encryption
- [ ] X11 Input Capture
- [ ] Latency measurement and visualization
- [ ] Bandwidth usage measurement and visualization
- [ ] Clipboard support


## Detailed OS Support

In order to use a device for sending events, an **input-capture** backend is required, while receiving events requires
a supported **input-emulation** *and* **input-capture** backend.

A suitable backend is chosen automatically based on the active desktop environment / compositor.

The following sections detail the emulation and capture backends provided by syntra and their support in desktop environments / operating systems.

### Input Emulation Support

| Desktop / Backend         | wlroots                  | libei                    | remote-desktop portal    | windows                  |   macos                                | x11                |
|---------------------------|--------------------------|--------------------------|--------------------------|--------------------------|----------------------------------------|--------------------|
| Wayland (wlroots)         | :heavy_check_mark:       |                          |                          |                          |                                        |                    |
| Wayland (KDE)             |                          | :heavy_check_mark:       | :heavy_check_mark:       |                          |                                        |                    |
| Wayland (Gnome)           |                          | :heavy_check_mark:       | :heavy_check_mark:       |                          |                                        |                    |
| Windows                   |                          |                          |                          | :heavy_check_mark:       |                                        |                    |
| MacOS                     |                          |                          |                          |                          |   :heavy_check_mark:                   |                    |
| X11                       |                          |                          |                          |                          |                                        | :heavy_check_mark: |

- `wlroots`: This backend makes use of the [wlr-virtual-pointer-unstable-v1](https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1) and [virtual-keyboard-unstable-v1](https://wayland.app/protocols/virtual-keyboard-unstable-v1) protocols and is supported by most wlroots based compositors.
- `libei`: This backend uses [libei](https://gitlab.freedesktop.org/libinput/libei) and is supported by GNOME >= 45 or KDE Plasma >= 6.1.
- `xdp`: This backend uses the [freedesktop remote-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/#gdbus-org.freedesktop.portal.RemoteDesktop) and is supported on GNOME and Plasma.
- `x11`: Backend for X11 sessions.
- `windows`: Backend for Windows.
- `macos`: Backend for MacOS.



### Input Capture Support

| Desktop / Backend         | layer-shell              | libei                    | windows                  |   macos                                | x11 |
|---------------------------|--------------------------|--------------------------|--------------------------|----------------------------------------|-----|
| Wayland (wlroots)         | :heavy_check_mark:       |                          |                          |                                        |     |
| Wayland (KDE)             | :heavy_check_mark:       | :heavy_check_mark:       |                          |                                        |     |
| Wayland (Gnome)           |                          | :heavy_check_mark:       |                          |                                        |     |
| Windows                   |                          |                          | :heavy_check_mark:       |                                        |     |
| MacOS                     |                          |                          |                          |   :heavy_check_mark:                   |     |
| X11                       |                          |                          |                          |                                        | WIP |

- `layer-shell`: This backend creates a single pixel wide window on the edges of Displays to capture the cursor using the [layer-shell protocol](https://wayland.app/protocols/wlr-layer-shell-unstable-v1).
- `libei`: This backend uses [libei](https://gitlab.freedesktop.org/libinput/libei) and is supported by GNOME >= 45 or KDE Plasma >= 6.1.
- `windows`: Backend for input capture on Windows.
- `macos`: Backend for input capture on MacOS.
- `x11`: TODO (not yet supported)
