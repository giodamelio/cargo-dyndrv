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
      forAllSystems = f: lib.genAttrs lib.systems.flakeExposed (system: f (makePkgs system));
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            # Use unstable cargo to be safe.
            # Technically we could use __CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS=nightly instead
            (pkgs.fenix.complete.withComponents [
              "cargo"
              "clippy"
              "rust-src"
              "rustc"
              "rustfmt"
            ])
            pkgs.rust-analyzer-nightly
            pkgs.gdb
          ];

          BUILD_WRAP = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.build-wrap;
          ENV_WRAP = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.env-wrap;
        };
      });

      packages = forAllSystems (pkgs: {
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

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
