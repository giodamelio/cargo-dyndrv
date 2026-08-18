{
  lib,
  stdenv,
  runCommand,
  rustPlatform,
  cargo,
  writeExtern,
  cargo-dyndrv,
}:
{
  extern ? { },
  pname ? "",
  version ? "",
  name ? "${pname}-${version}",
  outputs,
  ...
}@args:
let
  dyndrvAsCargo = runCommand "cargo-dyndrv-cargo" { } ''
    mkdir -p $out/bin
    ln -s ${lib.getExe cargo-dyndrv} $out/bin/cargo
  '';

  baseDerivation = rustPlatform.buildRustPackage (
    args
    // {
      name = "cargo-dyndrv-build.drv";

      env = (args.env or { }) // {
        CARGO = lib.getExe cargo;
        EXTERN_PATH = writeExtern extern;
      };
      nativeBuildInputs = (args.nativeBuildInputs or [ ]) ++ [
        dyndrvAsCargo
      ];

      # It's only possible to send one output,
      # but the generated derivation may have multiple
      outputs = [ "out" ];

      doCheck = false;
      dontInstall = false;
      dontCargoInstall = true;
      dontFixup = true;

      requiredSystemFeatures = [ "builder-rpc-v0" ];

      # nixpgks stdenv assumes $out exists, but it does not with builder-rpc-v0
      out = "/nonexistant";

      __contentAddressed = true;
      outputHashMode = "text";
      outputHashAlgo = "sha256";

    }
  );

in
runCommand name
  {
    preferLocalBuild = true;
    passthru.base = baseDerivation;
    inherit outputs;

    out = "/nonexistant";

    __contentAddressed = true;
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
  }
  (
    lib.concatMapStringsSep "\n" (output: ''
      ln -sL ${builtins.outputOf baseDerivation.outPath output} ${builtins.placeholder output}
    '') outputs
  )
