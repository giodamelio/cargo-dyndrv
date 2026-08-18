{
  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      fenix,
      self,
    }:
    let
      inherit (nixpkgs) lib;
      makePkgs =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ fenix.overlays.default ];
        };
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

            # Use unstable cargo to be safe.
            # Technically we could use __CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS=nightly instead
            CARGO = lib.getExe' pkgs.fenix.complete.cargo "cargo";

            BUILD_WRAP = lib.getExe self.packages.${system}.build-wrap;
            ENV_WRAP = lib.getExe self.packages.${system}.env-wrap;
          };
        }
      );

      packages = forAllSystems (system: pkgs: {
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
      });

      formatter = forAllSystems (system: pkgs: pkgs.nixfmt-tree);
    };
}
