# cargo-dyndrv
This repo contains a tool to build Rust programs in Nix with dynamic derivations.
`cargo-dyndrv` uses the Cargo unit graph to generate a minimal Nix minimal derivation
for each crate, adding only the necessary dependencies.

All generated derivations are content-addressing, which can can reduce rebuilds when a crate's source code or build
flags change but the resulting binary is identical.

- [cargo-dyndrv: A Beginning](https://blog.obsidian.systems/cargo-dyndrv-a-beginning/) (2026-09-16)

## Inside a derivation
A simple example is included in the [ffmpeg-example](https://github.com/obsidiansystems/cargo-dyndrv/tree/ffmpeg-example)
branch.

Enable the `cargo-dyndrv.overlays.default` overlay from `cargo-dyndrv` flake and
replace instances of `rustPlatform.buildRustPackage` with `pkgs.buildDynamicCrate`.

Then, add an argument `outputs = ["crate-name"];` with the name of the built crate.

If needed, add dependencies of each build script to a new `extern` field.
The keys are the Cargo crate IDs, and the values are attrsets that may contain
`path` and `env` keys for extra directories to add to the path and environment variables to set.

An entry for a crate that uses pkg-config might look like:
```nix
"registry+https://github.com/rust-lang/crates.io-index#ffmpeg-sys-next@9.0.0" = {
  path = [ "${lib.getBin pkgs.buildPackages.pkg-config}/bin" ];
  env = {
    PKG_CONFIG_PATH = "${lib.getDev pkgs.ffmpeg-headless}/lib/pkgconfig";
    PKG_CONFIG = lib.getExe pkgs.buildPackages.pkg-config;
  };
};
```

## Outside a derivation
It is possible to run cargo-dyndrv outside of a derivation for testing or development.

Build `cargo-dyndrv` with a standard `cargo build` in the default devShell or with the `.#cargo-dyndrv` flake package.

Then, with `rustc` and `cargo` in the PATH, run normal Cargo commands with `cargo-dyndrv` instead of `cargo`.

It will print out derivation paths, which can be built with Nix.

For example:
```shell
drvPath="$(nix run github:obsidiansystems/cargo-dyndrv#cargo-dyndrv -- build)"
nix build --print-out-paths "$drvPath^*"
```

If external dependencies are required, they must be converted into a JSON format then passed to `cargo-dyndrv`.

Pass values in the same format as the `extern` field when building inside a derivation to the `writeExtern` function,
then set the `EXTERN_PATH` environment variable to its path or copy it to `extern.json` in the current directory.

## Dependencies
`cargo-dyndrv` always generates content-addressing derivations,
which require the `ca-derivations` experimental feature in the Nix daemon.

When running inside a derivation, `cargo-dyndrv` requires the `dynamic-derivations` experimental feature to build the
generated derivations and a sufficiently recent version of Nix to add derivations to the store.

At time of writing, the latest stable version of Nix is 2.35, which does not include the necessary changes.
Before Nix 2.36 is released, only git versions of Nix are supported.

## Platform support
Due to limited hardware and time, `cargo-dyndrv` has only been tested on x86_64 Linux machines,
both natively and cross-compiled for AArch64 Linux. However, we expect it should work on other Linux architectures.

macOS is unlikely to work properly due to hardcoded name assumptions in the code and lack of support for the special
"Framework" libraries. This may be changed in the future.

## Blog posts

As we work on `cargo-dyndrv`, we aim to write up what we are doing in a series of blog posts.

---

<p align="center">
  <img src="logos.svg" alt="Obsidian Systems × Saronic" width="70%">
</p>
