{
  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";
  };

  outputs =
    {
      nixpkgs,
      self,
    }:
    let
      inherit (nixpkgs) lib;
      makePkgs = system: import nixpkgs { inherit system; };
      forAllSystems = f: lib.genAttrs lib.systems.flakeExposed (system: f system (makePkgs system));
    in
    {
      devShells = forAllSystems (
        system: pkgs: {
          default = pkgs.mkShell {
            packages = with pkgs; [
              # Use nixpkgs rust, fenix rust uses an integrated ld that doesn't set proper rpaths
              rustc
              cargo
              rustfmt
              clippy

              gdb
              rust-analyzer
            ];

            # For some reason nixpkgs cargo will accept unstable features.
            # If it didn't we could use __CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS=nightly
            # or cargo (but not rustc) from fenix
            BUILD_WRAP = lib.getExe self.packages.${system}.build-wrap;
            ENV_WRAP = lib.getExe self.packages.${system}.env-wrap;
          };
        }
      );

      packages = forAllSystems (
        system: pkgs: rec {
          # these two only use stdlib, no need to use cargo
          build-wrap = pkgs.buildRustCrate {
            crateName = "build-wrap";
            version = "0.1.0";
            src = ./build-wrap;
            crateBin = [ { name = "build-wrap"; } ];
          };
          env-wrap = pkgs.buildRustCrate {
            crateName = "env-wrap";
            version = "0.1.0";
            src = ./env-wrap;
            crateBin = [ { name = "env-wrap"; } ];
          };

          cargo-dyndrv = pkgs.rustPlatform.buildRustPackage rec {
            pname = "cargo-dyndrv";
            version = "0.1.0";

            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes."harmonia-file-core-3.1.0" = "sha256-hPdQB+DWc/q6w/wnTVGCS8iugEGd60Ym3x2eWvVw3ss=";
            };

            cargoBuildFlags = [
              "--bin"
              pname
            ];

            env = {
              BUILD_WRAP = lib.getExe build-wrap;
              ENV_WRAP = lib.getExe env-wrap;
              LN = lib.getExe' pkgs.coreutils "ln";
            };
          };

          writeExtern = pkgs.callPackage ./nix/write-extern.nix { };
        }
      );

      formatter = forAllSystems (system: pkgs: pkgs.nixfmt-tree);
    };
}
