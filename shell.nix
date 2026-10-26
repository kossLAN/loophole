let
  locked = (builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.nixpkgs.locked;
  nixpkgs = fetchTarball {
    url = "https://github.com/${locked.owner}/${locked.repo}/archive/${locked.rev}.tar.gz";
    sha256 = locked.narHash;
  };
  scope = import ./nix {
    inherit nixpkgs;
    system = builtins.currentSystem;
  };
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
