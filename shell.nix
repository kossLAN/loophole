{
  nixpkgs ? <nixpkgs>,
  system ? builtins.currentSystem,
}:

let
  scope = import ./nix { inherit nixpkgs system; };
in
scope.callPackage (
  {
    lib,
    mkShell,
    rustc,
    cargo,
    clippy,
    rustfmt,
    pkg-config,
    fontconfig,
    keymaps,
    loophole,
    loophole-unwrapped,
  }:

  mkShell {
    packages = [
      rustc
      cargo
      clippy
      rustfmt
      pkg-config
    ];
    buildInputs = [ fontconfig ];
    env = loophole.wrapperEnv // {
      LOOPHOLE_KEYMAPS_CSV = "${keymaps}";
      LD_LIBRARY_PATH = lib.makeLibraryPath loophole-unwrapped.runtimeDependencies;
    };
  }
) { }
