{
  stdenv,
  rustPlatform,
  lib,
  pkg-config,
  libX11,
  libXcursor,
  libXi,
  libXrandr,
  libxcb,
  libxkbcommon,
  wayland,
  libGL,
  fontconfig,
  gtk4,
  libXtst,
  librsvg,
  git,
  wrapGAppsHook4,
}:
let
  cargoToml = fromTOML (builtins.readFile ../Cargo.toml);
  pname = "syntra";
  version = cargoToml.workspace.package.version;
in
rustPlatform.buildRustPackage {
  inherit pname;
  inherit version;
  nativeBuildInputs = [
    pkg-config
    git
  ] ++ lib.optionals stdenv.isLinux [
    wrapGAppsHook4
  ];

  buildInputs = [
    fontconfig
  ]
  ++ lib.optionals stdenv.isLinux [
    gtk4
    librsvg
    libX11
    libXtst
    libXcursor
    libXi
    libXrandr
    libxcb
    libxkbcommon
    wayland
    libGL
  ];

  src = builtins.path {
    name = pname;
    path = lib.cleanSource ../.;
  };

  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [
    "-p"
    "syntra-app"
    "-p"
    "syntra-daemon"
  ] ++ lib.optionals stdenv.isLinux [
    "-p"
    "syntra-plugin-clipboard"
    "-p"
    "syntra-plugin-fuse"
    "-p"
    "syntra-plugin-edge-glow"
  ];

  # Set Environment Variables
  RUST_BACKTRACE = "full";
  # The sandbox has no session: socket paths resolve from XDG_RUNTIME_DIR.
  preCheck = ''
    export XDG_RUNTIME_DIR=$(mktemp -d)
  '';
  postInstall = ''
    ${lib.optionalString stdenv.isLinux ''
      test -x $out/bin/syntra-plugin-clipboard
      test -x $out/bin/syntra-plugin-fuse
      test -x $out/bin/syntra-plugin-edge-glow
    ''}
    test -x $out/bin/syntra-daemon
    install -Dm444 *.desktop -t $out/share/applications
    install -Dm444 crates/syntra-ui/ui/assets/shell/syntra.svg $out/share/icons/hicolor/scalable/apps/io.syntra.Syntra.svg
  '';

  meta = with lib; {
    description = "Syntra is a mouse and keyboard sharing software";
    longDescription = ''
      Syntra is a mouse and keyboard sharing software similar to universal-control on Apple devices. It allows for using multiple pcs with a single set of mouse and keyboard. This is also known as a Software KVM switch.
      The primary target is Wayland on Linux but Windows and MacOS and Linux on Xorg have partial support as well (see below for more details).
    '';
    mainProgram = pname;
    platforms = platforms.all;
  };
}
