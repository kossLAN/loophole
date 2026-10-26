{
  lib,
  stdenvNoCC,
  makeBinaryWrapper,
  copyDesktopItems,
  makeDesktopItem,
  swtpm,
  loophole-unwrapped,
  qemu,
  ovmf,
  # Unset, loophole downloads the latest build of kossLAN/loophole-guest.
  driver-disc ? null,
}:

let
  wrapperEnv = {
    LOOPHOLE_QEMU = "${qemu}/bin/qemu-system-x86_64";
    LOOPHOLE_QEMU_IMG = "${qemu}/bin/qemu-img";
    LOOPHOLE_OVMF_CODE = "${ovmf}/FV/OVMF_CODE.fd";
    LOOPHOLE_OVMF_VARS = "${ovmf}/FV/OVMF_VARS.fd";
    LOOPHOLE_SWTPM = "${swtpm}/bin/swtpm";
  }
  // lib.optionalAttrs (driver-disc != null) {
    LOOPHOLE_DRIVER_DISC = "${driver-disc}";
  };

  wrapperArgs = lib.mapAttrsToList (name: value: "--set-default ${name} ${value}") wrapperEnv;
in
stdenvNoCC.mkDerivation {
  pname = "loophole";
  inherit (loophole-unwrapped) version;

  dontUnpack = true;

  nativeBuildInputs = [
    makeBinaryWrapper
    copyDesktopItems
  ];

  desktopItems = [
    (makeDesktopItem {
      name = "loophole";
      desktopName = "loophole";
      comment = loophole-unwrapped.meta.description;
      exec = "loophole";
      icon = "computer";
      categories = [
        "System"
        "Emulator"
      ];
    })
  ];

  installPhase = ''
    runHook preInstall
    makeBinaryWrapper ${lib.getExe loophole-unwrapped} $out/bin/loophole \
      ${lib.concatStringsSep " " wrapperArgs}
    runHook postInstall
  '';

  passthru = { inherit wrapperEnv; };

  meta = loophole-unwrapped.meta // {
    description = "Runs Windows VMs on the host's AMD GPU and shows them";
  };
}
