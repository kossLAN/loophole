use loophole_vm::config::{DiskSource, Nic, VmConfig};
use loophole_vm::dirs;
use loophole_vm::host;
use slint::{ComponentHandle as _, SharedString};

use crate::models::Tracked;
use crate::ui::{
    BootEdit, BootKind, DiskEdit, DiskKind, Edit, ForwardEdit, NicEdit, PciEdit, UsbEdit, VmEdit,
};

use super::fields::{gib, gpu_window_index, index, int, text};
use super::{Editor, NEW_DISK_GIB};

impl Editor {
    pub(super) fn show(&mut self, cfg: &VmConfig) {
        let Some(ui) = self.ui.upgrade() else { return };
        let edit = ui.global::<Edit>();
        edit.set_host_storage(text(
            host::free_space(&dirs::data_dir(&cfg.name))
                .map_or_else(String::new, |free| format!("Available: {}", gib(free))),
        ));
        edit.set_vm(VmEdit {
            name: text(cfg.name.as_str()),
            windows: index(cfg.guest.windows.index()),
            sockets: int(cfg.cpu.sockets),
            cores: int(cfg.cpu.cores),
            threads: int(cfg.cpu.threads),
            manual_topology: cfg.cpu.sockets != 1 || cfg.cpu.threads != 1,
            hyperv: cfg.cpu.hyperv,
            memory_gib: int(cfg.memory.size_gib),
            hugepages: index(cfg.memory.hugepages.index()),
            gpu_window: gpu_window_index(&self.gpu_window_sizes, cfg.gpu.window_gib),
            install: cfg
                .media
                .install
                .as_deref()
                .map(|p| text(p.display().to_string()))
                .unwrap_or_default(),
            driver_disc: cfg.media.driver_disc,
            sound: cfg.sound.enabled,
            microphone: cfg.sound.microphone,
            tablet: cfg.input.tablet,
            boot_menu: cfg.boot.menu,
            boot_timeout: int(cfg.boot.menu_timeout_ms),
            resolution: cfg
                .display
                .resolution
                .map(|r| text(r.to_string()))
                .unwrap_or_default(),
            refresh: cfg
                .display
                .refresh_hz
                .map(|r| text(r.to_string()))
                .unwrap_or_default(),
            capture: index(cfg.display.capture.index()),
            clipboard: cfg.display.clipboard,
            tpm: cfg.machine.tpm,
            machine_options: text(cfg.machine.options.as_str()),
            extra_args: text(cfg.machine.extra_args.join(" ")),
        });

        let disks: Vec<DiskEdit> = cfg
            .disks
            .iter()
            .map(|d| {
                let (kind, path) = match &d.source {
                    DiskSource::Image(p) => (DiskKind::Image, p),
                    DiskSource::Device(p) => (DiskKind::Device, p),
                };

                DiskEdit {
                    id: self.id(),
                    kind,
                    path: text(path.display().to_string()),
                    new_gib: NEW_DISK_GIB,
                    bus: index(d.bus.index()),
                    format: index(d.format.index()),
                    cache: index(d.cache.index()),
                    discard: d.discard,
                    serial: d.serial.as_deref().map(text).unwrap_or_default(),
                    ..DiskEdit::default()
                }
            })
            .collect();
        self.disks.rows().set_vec(disks.clone());
        self.refresh_disk_titles();

        let nics: Vec<NicEdit> = cfg.nics.iter().map(|n| self.nic_edit(n)).collect();
        self.nics.rows().set_vec(nics);

        let pci: Vec<PciEdit> = cfg
            .pci
            .iter()
            .map(|p| PciEdit {
                id: self.id(),
                address: text(p.address.to_string()),
                rom_bar: p.rom_bar,
                ..PciEdit::default()
            })
            .collect();
        self.pci.rows().set_vec(pci.clone());
        self.refresh_pci();

        let usb_present = host::usb_devices();
        self.usb.set_vec(
            cfg.usb
                .iter()
                .map(|u| UsbEdit {
                    id: text(u.id.to_string()),
                    name: text(
                        u.name
                            .clone()
                            .or_else(|| {
                                usb_present
                                    .iter()
                                    .find(|d| d.id == u.id)
                                    .map(|d| d.name.clone())
                            })
                            .unwrap_or_else(|| u.id.to_string()),
                    ),
                    status: text(if usb_present.iter().any(|d| d.id == u.id) {
                        ""
                    } else {
                        "not plugged in"
                    }),
                })
                .collect::<Vec<_>>(),
        );

        let mut boot: Vec<(u32, BootEdit)> = Vec::new();
        let mut rest = Vec::new();

        for (d, row) in cfg.disks.iter().zip(&disks) {
            let entry = BootEdit {
                kind: BootKind::Disk,
                id: row.id,
                enabled: d.boot_order.is_some(),
                ..BootEdit::default()
            };

            match d.boot_order {
                Some(n) => boot.push((n, entry)),
                None => rest.push(entry),
            }
        }

        let install = BootEdit {
            kind: BootKind::InstallDrive,
            id: 0,
            enabled: cfg.media.install_boot_order.is_some(),
            ..BootEdit::default()
        };

        match cfg.media.install_boot_order {
            Some(n) => boot.push((n, install)),
            None => rest.push(install),
        }

        for (p, row) in cfg.pci.iter().zip(&pci) {
            let entry = BootEdit {
                kind: BootKind::Pci,
                id: row.id,
                enabled: p.boot_order.is_some(),
                ..BootEdit::default()
            };

            match p.boot_order {
                Some(n) => boot.push((n, entry)),
                None => rest.push(entry),
            }
        }

        boot.sort_by_key(|(n, _)| *n);
        let boot: Vec<BootEdit> = boot.into_iter().map(|(_, b)| b).chain(rest).collect();
        self.boot.rows().set_vec(boot);
        self.refresh_boot_labels();
    }

    pub(super) fn nic_edit(&self, n: &Nic) -> NicEdit {
        let forwards = Tracked::new(self.on_edit.clone());
        forwards.rows().set_vec(
            n.forwards
                .iter()
                .map(|f| ForwardEdit {
                    proto: index(f.proto.index()),
                    address: f.address.map(|a| text(a.to_string())).unwrap_or_default(),
                    host: i32::from(f.host),
                    guest: i32::from(f.guest),
                })
                .collect::<Vec<_>>(),
        );

        NicEdit {
            model: index(n.model.index()),
            backend: index(n.backend.index()),
            tap: n.tap.as_deref().map(text).unwrap_or_default(),
            mac: n.mac.map(|m| text(m.to_string())).unwrap_or_default(),
            forwards: forwards.model(),
            error: SharedString::new(),
        }
    }
}
