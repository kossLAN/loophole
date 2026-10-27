{ nixpkgs, system }:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.lib.makeScope pkgs.newScope (self: {
  host = {
    virglrenderer = self.callPackage ./virglrenderer { };
    qemu = self.callPackage ./qemu { inherit (self.host) virglrenderer; };
    ovmf = self.callPackage ./ovmf.nix { };
  };
  loophole-unwrapped = self.callPackage ./loophole.nix { };
  loophole = self.callPackage ./wrapper.nix {
    inherit (self.host) qemu ovmf;
  };
})
