{ lib, virglrenderer }:

virglrenderer.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ lib.filesystem.listFilesRecursive ./patches;
})
