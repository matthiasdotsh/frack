{
  description = "Sheet music viewer for Linux: half-page turns, stylus annotations burned into the PDF, tuner with pitch history";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # For SBOM generation (nix run .#sbom); the nixpkgs version of
    # sbomnix does not understand the derivation JSON of current Nix.
    sbomnix = {
      url = "github:tiiuae/sbomnix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      sbomnix,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      # For use in other configurations:
      #   nixpkgs.overlays = [ frack.overlays.default ];
      # or install the package output directly:
      #   environment.systemPackages = [ frack.packages.${system}.default ];
      overlays.default = final: _prev: {
        frack = final.callPackage ./nix/package.nix { };
      };

      packages = forAllSystems (
        pkgs:
        let
          frack = pkgs.callPackage ./nix/package.nix { };
          sbom-rust = pkgs.callPackage ./nix/sbom.nix { };
          sbom = pkgs.callPackage ./nix/sbom-app.nix {
            flake = self;
            sbomnix = sbomnix.packages.${pkgs.stdenv.hostPlatform.system}.sbomnix;
            inherit sbom-rust;
          };
        in
        {
          inherit frack sbom sbom-rust;
          default = frack;
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ (pkgs.callPackage ./nix/package.nix { }) ];
          packages = [
            pkgs.rustfmt
            pkgs.clippy
            pkgs.rust-analyzer
            pkgs.poppler-utils # pdftoppm, for inspecting burn_demo output
          ];
          env.RUST_BACKTRACE = "1";
        };
      });
    };
}
