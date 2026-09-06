self: {
  config,
  pkgs,
  lib,
  ...
}:
with lib; let
  cfg = config.programs.syntra;
  defaultPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  tomlFormat = pkgs.formats.toml {};
in {
  options.programs.syntra = with types; {
    enable = mkEnableOption "Whether or not to enable syntra.";
    package = mkOption {
      type = with types; nullOr package;
      default = defaultPackage;
      defaultText = literalExpression "inputs.syntra.packages.${pkgs.stdenv.hostPlatform.system}.default";
      description = ''
        The syntra package to use.

        By default, this option will use the `packages.default` as exposed by this flake.
      '';
    };
    systemd = mkOption {
      type = types.bool;
      default = pkgs.stdenv.isLinux;
      description = "Whether to enable to systemd service for syntra on linux.";
    };
    launchd = mkOption {
      type = types.bool;
      default = pkgs.stdenv.isDarwin;
      description = "Whether to enable to launchd service for syntra on macOS.";
    };
    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = {};
      example = builtins.fromTOML (builtins.readFile (self + /config.toml));
      description = ''
        Optional configuration written to {file}`$XDG_CONFIG_HOME/syntra/config.toml`.

        See <https://github.com/DarkPhilosophy/syntra/> for
        available options and documentation.
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.user.services.syntra = lib.mkIf cfg.systemd {
      Unit = {
        Description = "Systemd service for Syntra";
        Requires = ["graphical-session.target"];
      };
      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/syntra daemon";
      };
      Install.WantedBy = [
        (lib.mkIf config.wayland.windowManager.hyprland.systemd.enable "hyprland-session.target")
        (lib.mkIf config.wayland.windowManager.sway.systemd.enable "sway-session.target")
      ];
    };

    launchd.agents.syntra = lib.mkIf cfg.launchd {
      enable = true;
      config = {
        ProgramArguments = [
          "${cfg.package}/bin/syntra"
          "daemon"
        ];
        KeepAlive = true;
      };
    };

    home.packages = [
      cfg.package
    ];

    xdg.configFile."syntra/config.toml" = lib.mkIf (cfg.settings != {}) {
      source = tomlFormat.generate "config.toml" cfg.settings;
    };
  };
}
