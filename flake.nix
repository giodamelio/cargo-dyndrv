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
      makePkgs =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ self.overlays.default ];
        };
      forAllSystems = f: lib.genAttrs lib.systems.flakeExposed (system: f (makePkgs system));
    in
    {
      devShells = forAllSystems (pkgs: {
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
          BUILD_WRAP = lib.getExe pkgs.build-wrap;
          ENV_WRAP = lib.getExe pkgs.env-wrap;
          TARGET_ENV = lib.getExe pkgs.target-env;
        };
        cross =
          let
            pkgs' = pkgs.pkgsCross.aarch64-multiplatform;
          in
          pkgs'.stdenv.mkDerivation rec {
            name = "shell-cross";
            nativeBuildInputs = with pkgs'; [
              rustc
              cargo
              rustfmt
              clippy

              gdb
              rust-analyzer
            ];

            HOST_CC = lib.getExe pkgs'.buildPackages.stdenv.cc;
            HOST_CXX = lib.getExe' pkgs'.buildPackages.stdenv.cc "${pkgs'.buildPackages.stdenv.cc.targetPrefix}c++";

            "CARGO_TARGET_${pkgs'.stdenv.buildPlatform.rust.cargoEnvVarTarget}_LINKER" = HOST_CC;
            "CARGO_TARGET_${pkgs'.hostPlatform.rust.cargoEnvVarTarget}_LINKER" = lib.getExe pkgs'.stdenv.cc;

            BUILD_WRAP = lib.getExe pkgs'.build-wrap;
            ENV_WRAP = lib.getExe pkgs'.env-wrap;
            TARGET_ENV = lib.getExe pkgs'.target-env;
          };
      });

      overlays.default =
        final: prev:
        let
          # these only use stdlib, no need to use cargo
          makeHelper =
            name:
            final.buildRustCrate {
              crateName = name;
              version = "0.1.0";
              src = ./${name};
              crateBin = [ { inherit name; } ];
            };
        in
        {
          build-wrap = makeHelper "build-wrap";
          env-wrap = makeHelper "env-wrap";
          target-env = makeHelper "target-env";

          cargo-dyndrv = final.rustPlatform.buildRustPackage rec {
            pname = "cargo-dyndrv";
            version = "0.1.0";

            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./cargo-dyndrv
                # cargo insists on seeing all members of a workspace,
                # even when resolving only one of them.
                ./env-wrap
                ./target-env
                ./build-wrap
              ];
            };
            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes."harmonia-file-core-3.1.0" = "sha256-hPdQB+DWc/q6w/wnTVGCS8iugEGd60Ym3x2eWvVw3ss=";
            };

            cargoBuildFlags = [
              "--bin"
              pname
            ];

            env = {
              BUILD_WRAP = lib.getExe final.build-wrap;
              ENV_WRAP = lib.getExe final.env-wrap;
              TARGET_ENV = lib.getExe final.target-env;
              LN = lib.getExe' final.coreutils "ln";
            };

            meta.mainProgram = "cargo-dyndrv";
          };

          writeExtern = final.callPackage ./nix/write-extern.nix { };
          buildCrate = final.callPackage ./nix/build-crate.nix { };
        };

      packages = forAllSystems (pkgs: {
        inherit (pkgs)
          build-wrap
          env-wrap
          target-env
          cargo-dyndrv
          writeExtern
          buildCrate
          ;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
