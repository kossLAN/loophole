{
  # The patch sets in nix/qemu and nix/virglrenderer target this nixpkgs revision.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/151fa4e8ddfdd8dd25d945ad94ed54a13de9f6e4";

  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [ "x86_64-linux" ];
      eachSystem = f: lib.genAttrs systems (system: f self.legacyPackages.${system});
    in
    {
      legacyPackages = lib.genAttrs systems (system: import ./nix { inherit nixpkgs system; });

      packages = eachSystem (w: {
        default = w.loophole;
        inherit (w) loophole;
        inherit (w.host) qemu virglrenderer;
      });

      checks = eachSystem (w: {
        inherit (w) loophole;
      });

      formatter = lib.genAttrs systems (system: nixpkgs.legacyPackages.${system}.nixfmt-tree);

      overlays.default = final: _prev: {
        inherit (self.packages.${final.stdenv.hostPlatform.system}) loophole;
      };

      nixosModules.default =
        { pkgs, lib, ... }:
        {
          imports = [ ./nix/module.nix ];
          programs.loophole.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.loophole;
        };
    };
}
