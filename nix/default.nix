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
  pname = cargoToml.package.name;
  version = cargoToml.package.version;
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
    "syntra"
  ] ++ lib.optionals stdenv.isLinux [
    "-p"
    "syntra-plugin-gtk-clipboard"
    "-p"
    "syntra-plugin-fuse"
  ];

  # Set Environment Variables
  RUST_BACKTRACE = "full";
  postInstall = ''
    ${lib.optionalString stdenv.isLinux ''
      test -x $out/bin/syntra-plugin-gtk-clipboard
      test -x $out/bin/syntra-plugin-fuse
    ''}
    install -Dm444 *.desktop -t $out/share/applications
    install -Dm444 syntra-ui/ui/assets/shell/syntra.svg $out/share/icons/hicolor/scalable/apps/syntra.svg
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
