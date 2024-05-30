{
  description = "GReX T0 Nix Flake";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    flake-utils.url = "github:numtide/flake-utils";

    crane = {
      url = "github:ipetkov/crane";
      inputs = {
        nixpkgs.follows = "nixpkgs";
      };
    };

    fenix = {
      url = "github:nix-community/fenix";
      inputs = {
        nixpkgs.follows = "nixpkgs";
      };
    };

    psrdada = {
      url = "github:kiranshila/psrdada.nix";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        flake-utils.follows = "flake-utils";
      };
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    psrdada,
    crane,
    fenix,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [fenix.overlays.default];
      };

      inherit (pkgs) lib;

      craneLib =
        (crane.mkLib pkgs).overrideToolchain
        (pkgs.fenix.complete.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
        ]);

      # T0 depends on an fpg file to build the SNAP interface,
      # so that must be deterministically included as well
      fpgFilter = path: _type: null != builtins.match ".*fpg$" path;
      fpgOrCargo = path: type: (fpgFilter path type) || (craneLib.filterCargoSources path type);
      src = lib.cleanSourceWith {
        src = craneLib.path ./.; # The original, unfiltered source
        filter = fpgOrCargo;
      };

      commonArgs = {
        inherit src;

        nativeBuildInputs = with pkgs; [
          rustPlatform.bindgenHook
          pkg-config
        ];

        buildInputs = with pkgs; [
          netcdf
          hdf5
          psrdada.packages.${system}.default
        ];
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      my-crate = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
          NIX_OUTPATH_USED_AS_RANDOM_SEED = "plsfindfrb";
        });
    in {
      checks = {
        inherit my-crate;
        my-crate-clippy = craneLib.cargoClippy (commonArgs
          // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });
        my-crate-fmt = craneLib.cargoFmt {inherit src;};
      };

      packages = {
        default = my-crate;
        inherit my-crate;
      };

      apps.default = flake-utils.lib.mkApp {drv = my-crate;};

      devShells.default = craneLib.devShell {
        checks = self.checks.${system};
        packages = with pkgs; [
          alejandra
          cargo-machete
          cargo-show-asm
          cargo-outdated
          rust-analyzer-nightly
        ];
      };
    });
}
