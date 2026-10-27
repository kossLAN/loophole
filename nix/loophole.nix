{
  lib,
  stdenv,
  rustPlatform,
  autoPatchelfHook,
  pkg-config,
  fontconfig,
  libGL,
  libxkbcommon,
  wayland,
  libx11,
  libxcursor,
  libxi,
  libxrandr,
}:

rustPlatform.buildRustPackage {
  pname = "loophole-unwrapped";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    autoPatchelfHook
    pkg-config
  ];

  buildInputs = [
    fontconfig
    # libgcc_s, which autoPatchelfHook must find.
    (lib.getLib stdenv.cc.cc)
  ];

  # winit, glutin and khronos-egl open these at run time.
  runtimeDependencies = [
    libGL
    libxkbcommon
    wayland
    libx11
    libxcursor
    libxi
    libxrandr
  ];

  meta = {
    description = "Runs Windows VMs on the host's AMD GPU and shows them";
    # Slint itself is used under its royalty-free license.
    license = lib.licenses.mit;
    mainProgram = "loophole";
    platforms = [ "x86_64-linux" ];
  };
}
