# Nix Flake Usage

## Run

```bash
nix run github:DarkPhilosophy/syntra

# With params
nix run github:DarkPhilosophy/syntra -- --help

```

## Home-manager module

Add input:

```nix
inputs = {
    syntra.url = "github:DarkPhilosophy/syntra";
}
```

Optional: add [our binary cache](https://app.cachix.org/cache/syntra) to allow a faster package install.

```nix
nixConfig = {
    extra-substituters = [
        "https://syntra.cachix.org/"
    ];
    extra-trusted-public-keys = [
      "syntra.cachix.org-1:KlE2AEZUgkzNKM7BIzMQo8w9yJYqUpor1CAUNRY6OyM="
    ];
};
```

Enable syntra:

``` nix
{
  inputs,
  ...
}: {
  # Add the Home Manager module
  imports = [inputs.syntra.homeManagerModules.default];

  programs.syntra = {
    enable = true;
    # systemd = false;
    # package = inputs.syntra.packages.${pkgs.stdenv.hostPlatform.system}.default
    # Optional configuration in nix syntax, see config.toml for available options
    # settings = { };
    };
  };
}

```
