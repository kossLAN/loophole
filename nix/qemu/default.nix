{
  lib,
  qemu,
  virglrenderer,
}:

let
  base = qemu.override {
    hostCpuOnly = true;
    inherit virglrenderer;
  };
in
base.overrideAttrs (old: {
  patches = old.patches ++ lib.filesystem.listFilesRecursive ./patches;
})
