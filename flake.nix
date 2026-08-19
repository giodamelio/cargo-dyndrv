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
            TARGET_ENV = lib.getExe self.packages.${system}.target-env;
          };
        }
      );

      packages = forAllSystems (
        system: pkgs:
        let
          # these only use stdlib, no need to use cargo
          makeHelper =
            name:
            pkgs.buildRustCrate {
              crateName = name;
              version = "0.1.0";
              src = ./${name};
              crateBin = [ { inherit name; } ];
            };
        in
        rec {
          build-wrap = makeHelper "build-wrap";
          env-wrap = makeHelper "env-wrap";
          target-env = makeHelper "target-env";

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
              TARGET_ENV = lib.getExe target-env;
              LN = lib.getExe' pkgs.coreutils "ln";
            };

            meta.mainProgram = "cargo-dyndrv";
          };

          writeExtern = pkgs.callPackage ./nix/write-extern.nix { };
          buildCrate = pkgs.callPackage ./nix/build-crate.nix { inherit writeExtern cargo-dyndrv; };
        }
      );

      formatter = forAllSystems (system: pkgs: pkgs.nixfmt-tree);
    };
}
