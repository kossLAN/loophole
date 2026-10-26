{ config, lib, ... }:

let
  cfg = config.programs.loophole;
  inherit (lib) mkOption types;

  usbId = types.strMatching "[0-9a-f]{4}:[0-9a-f]{4}";
  pciAddress = types.strMatching "[0-9a-f]{4}:[0-9a-f]{2}:[0-9a-f]{2}\\.[0-7]";

  diskRule =
    id:
    ''SUBSYSTEM=="block", ENV{DEVTYPE}=="disk", SYMLINK=="disk/by-id/${id}", GROUP="kvm", MODE="0660"'';

  usbRule =
    id:
    let
      vendor = lib.head (lib.splitString ":" id);
      product = lib.last (lib.splitString ":" id);
    in
    ''SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="${vendor}", ATTR{idProduct}=="${product}", GROUP="kvm", MODE="0660"'';

  # vfio-pci takes the device as it appears at boot, before the driver its model would get.
  vfioRule =
    address:
    ''ACTION=="add", SUBSYSTEM=="pci", KERNEL=="${address}", ATTR{driver_override}="vfio-pci"'';
  vfioRules = lib.concatLines (map vfioRule cfg.vfio.devices);
in
{
  options.programs.loophole = {
    enable = lib.mkEnableOption "loophole, Windows VMs on the host's AMD GPU";

    package = mkOption {
      type = types.package;
      description = "The loophole package.";
    };

    users = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "alice" ];
      description = "Users who run VMs, added to the kvm and render groups.";
    };

    vfio = {
      enable = lib.mkEnableOption "PCI passthrough: the IOMMU, vfio-pci and unlimited locked memory for the users";

      devices = mkOption {
        type = types.listOf pciAddress;
        default = [ ];
        example = [ "0000:0b:00.0" ];
        description = ''
          PCI devices bound to vfio-pci at boot, by address. Unlike `ids`, this leaves alone
          another device of the same model, such as the host's own NVMe drive.
        '';
      };

      ids = mkOption {
        type = types.listOf usbId;
        default = [ ];
        example = [ "144d:a80c" ];
        description = "Vendor:device ids that vfio-pci claims at boot, every device of each.";
      };
    };

    blockDevices = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "nvme-Samsung_SSD_990_PRO_2TB_S7KHNJ0X123456X" ];
      description = "Whole disks, by their /dev/disk/by-id name, that the users may give to VMs.";
    };

    usbDevices = mkOption {
      type = types.listOf usbId;
      default = [ ];
      example = [ "046d:c52b" ];
      description = "USB devices, by vendor:product id, that the users may give to VMs.";
    };

    hugepages = mkOption {
      type = types.nullOr (
        types.submodule {
          options = {
            size = mkOption {
              type = types.enum [
                "2M"
                "1G"
              ];
              description = "Page size.";
            };
            count = mkOption {
              type = types.ints.positive;
              description = "Pages reserved at boot.";
            };
          };
        }
      );
      default = null;
      example = {
        size = "1G";
        count = 16;
      };
      description = "A huge page pool for VMs set to use huge pages.";
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        environment.systemPackages = [ cfg.package ];

        users.users = lib.genAttrs cfg.users (_: {
          extraGroups = [
            "kvm"
            "render"
          ];
        });

        services.udev.extraRules = lib.concatLines (
          map diskRule cfg.blockDevices ++ map usbRule cfg.usbDevices
        );
      }

      (lib.mkIf cfg.vfio.enable {
        boot.kernelParams = [
          "iommu=pt"
          "intel_iommu=on"
        ]
        ++ lib.optional (cfg.vfio.ids != [ ]) "vfio-pci.ids=${lib.concatStringsSep "," cfg.vfio.ids}";

        boot.initrd.kernelModules = [
          "vfio_pci"
          "vfio"
          "vfio_iommu_type1"
        ];

        boot.extraModprobeConfig = ''
          softdep nvme pre: vfio-pci
          softdep xhci_pci pre: vfio-pci
          softdep ahci pre: vfio-pci
          softdep snd_hda_intel pre: vfio-pci
        '';

        # The initrd's drivers (NVMe for the root disk) probe before the system's udev runs.
        boot.initrd.services.udev.rules = vfioRules;

        services.udev.extraRules = ''
          SUBSYSTEM=="vfio", GROUP="kvm", MODE="0660"
          ${vfioRules}
        '';

        # VFIO pins all guest memory.
        security.pam.loginLimits = map (user: {
          domain = user;
          type = "-";
          item = "memlock";
          value = "unlimited";
        }) cfg.users;
        systemd.services."user@".serviceConfig.LimitMEMLOCK = "infinity";
        systemd.user.settings.Manager.DefaultLimitMEMLOCK = "infinity";
      })

      (lib.mkIf (cfg.hugepages != null) {
        boot.kernelParams = lib.mkIf (cfg.hugepages.size == "1G") [
          "hugepagesz=1G"
          "hugepages=${toString cfg.hugepages.count}"
        ];

        boot.kernel.sysctl = lib.mkIf (cfg.hugepages.size == "2M") {
          "vm.nr_hugepages" = cfg.hugepages.count;
        };
      })
    ]
  );
}
