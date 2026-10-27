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
      LD_LIBRARY_PATH = lib.makeLibraryPath loophole-unwrapped.runtimeDependencies;
    };
  }
) { }
