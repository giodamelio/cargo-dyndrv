{
  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";
  };

  outputs =
    { nixpkgs, ... }:
    let
      inherit (nixpkgs) lib;
      makePkgs = system: import nixpkgs { inherit system; };
      forAllSystems = f: lib.genAttrs lib.systems.flakeExposed (system: f (makePkgs system));
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            rust-analyzer
            clippy
            rustfmt

            pkg-config

            openssl
            curl
            libgit2
            libssh2
          ];
        };
      });
      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
