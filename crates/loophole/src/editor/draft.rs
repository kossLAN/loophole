use std::net::Ipv4Addr;
use std::path::PathBuf;

use loophole_vm::config::{
    self, Capture, Disk, DiskBus, DiskCache, DiskFormat, DiskSource, Forward, HugePages, Issue,
    Mac, NetBackend, Nic, NicModel, PciAddress, PciPassthrough, Proto, Resolution, Section, UsbId,
    UsbPassthrough, VmConfig, Windows,
};
use slint::{ComponentHandle as _, Model as _};

use crate::ui::{BootKind, DiskKind, Edit};

use super::fields::{count, optional};
use super::{Draft, Editor, Mode};

impl Editor {
    pub fn draft(&self, others: &[String]) -> Draft {
        let mut issues = Vec::new();
        let ui = self.ui.upgrade();
        let vm = ui
            .as_ref()
            .map(|ui| ui.global::<Edit>().get_vm())
            .unwrap_or_default();

        let name = match (&self.mode, &self.saved) {
            (Mode::Create, _) => {
                let name = vm.name.trim().to_owned();

                if name.is_empty() {
                    issues.push(Issue::error(Section::General, "name the VM"));
                } else if let Err(e) = config::check_name(&name) {
                    issues.push(Issue::error(Section::General, format!("{e:#}")));
                } else if others.contains(&name) {
                    issues.push(Issue::error(
                        Section::General,
                        format!("there is a VM named {name} already"),
                    ));
                }

                name
            },
            (_, Some(saved)) => saved.name.clone(),
            (_, None) => String::new(),
        };

        let mut cfg = self.saved.clone().unwrap_or_else(|| VmConfig::new(&name));
        cfg.name.clone_from(&name);

        cfg.cpu.sockets = count(vm.sockets);
        cfg.cpu.cores = count(vm.cores);
        cfg.cpu.threads = count(vm.threads);
        cfg.cpu.hyperv = vm.hyperv;
        cfg.memory.size_gib = count(vm.memory_gib);
        cfg.memory.hugepages = HugePages::from_index(count(vm.hugepages) as usize);

        if let Some(&size) = self.gpu_window_sizes.get(count(vm.gpu_window) as usize) {
            cfg.gpu.window_gib = size;
        }

        cfg.media.install = optional(&vm.install).map(PathBuf::from);
        cfg.media.driver_disc = vm.driver_disc;
        cfg.sound.enabled = vm.sound;
        cfg.sound.microphone = vm.microphone;
        cfg.input.tablet = vm.tablet;
        cfg.boot.menu = vm.boot_menu;
        cfg.boot.menu_timeout_ms = count(vm.boot_timeout);
        cfg.display.capture = Capture::from_index(count(vm.capture) as usize);
        cfg.display.clipboard = vm.clipboard;
        cfg.display.resolution = optional(&vm.resolution).and_then(|r| {
            r.parse::<Resolution>()
                .map_err(|e| issues.push(Issue::error(Section::Display, e)))
                .ok()
        });
        cfg.display.refresh_hz = optional(&vm.refresh).and_then(|r| {
            r.parse::<u32>()
                .map_err(|_| {
                    issues.push(Issue::error(
                        Section::Display,
                        format!("refresh rate: a number of Hz, not {r:?}"),
                    ));
                })
                .ok()
        });
        cfg.guest.windows = Windows::from_index(count(vm.windows) as usize);
        cfg.machine.tpm = vm.tpm;
        vm.machine_options
            .trim()
            .clone_into(&mut cfg.machine.options);
        cfg.machine.extra_args = vm.extra_args.split_whitespace().map(String::from).collect();

        let mut new_images = Vec::new();
        let mut planned: Vec<PathBuf> = Vec::new();
        let mut disk_ids = Vec::new();
        cfg.disks = Vec::new();

        for (i, d) in self.disks.to_vec().into_iter().enumerate() {
            let path = PathBuf::from(d.path.trim());

            let source = match d.kind {
                DiskKind::NewImage => {
                    let p = cfg.free_image_path(&planned);
                    planned.push(p.clone());
                    new_images.push((i, count(d.new_gib)));
                    DiskSource::Image(p)
                },
                DiskKind::Device => DiskSource::Device(path),
                DiskKind::Image => DiskSource::Image(path),
            };

            cfg.disks.push(Disk {
                source,
                bus: DiskBus::from_index(count(d.bus) as usize),
                format: DiskFormat::from_index(count(d.format) as usize),
                cache: DiskCache::from_index(count(d.cache) as usize),
                discard: d.discard,
                serial: optional(&d.serial),
                boot_order: None,
            });
            disk_ids.push(d.id);
        }

        cfg.nics = self
            .nics
            .to_vec()
            .into_iter()
            .enumerate()
            .map(|(i, n)| {
                let mac = optional(&n.mac).and_then(|m| {
                    m.parse::<Mac>()
                        .map_err(|e| issues.push(Issue::error(Section::Nic(i), e)))
                        .ok()
                });
                let forwards = n
                    .forwards
                    .iter()
                    .map(|f| Forward {
                        proto: Proto::from_index(count(f.proto) as usize),
                        address: optional(&f.address).and_then(|a| {
                            a.parse::<Ipv4Addr>()
                                .map_err(|_| {
                                    issues.push(Issue::error(
                                        Section::Nic(i),
                                        format!("{a} is not an IPv4 address"),
                                    ));
                                })
                                .ok()
                        }),
                        host: u16::try_from(f.host).unwrap_or(0),
                        guest: u16::try_from(f.guest).unwrap_or(0),
                    })
                    .collect();

                Nic {
                    model: NicModel::from_index(count(n.model) as usize),
                    backend: NetBackend::from_index(count(n.backend) as usize),
                    tap: optional(&n.tap),
                    mac,
                    forwards,
                }
            })
            .collect();

        let mut pci_ids = Vec::new();
        cfg.pci = self
            .pci
            .to_vec()
            .into_iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let address = p
                    .address
                    .parse::<PciAddress>()
                    .map_err(|e| issues.push(Issue::error(Section::Pci(i), e)))
                    .ok()?;
                pci_ids.push(p.id);

                Some(PciPassthrough {
                    address,
                    rom_bar: p.rom_bar,
                    boot_order: None,
                })
            })
            .collect();
        cfg.usb = self
            .usb
            .iter()
            .enumerate()
            .filter_map(|(i, u)| {
                let id =
                    u.id.parse::<UsbId>()
                        .map_err(|e| issues.push(Issue::error(Section::Usb(i), e)))
                        .ok()?;

                Some(UsbPassthrough {
                    id,
                    name: optional(&u.name).filter(|n| *n != id.to_string()),
                })
            })
            .collect();

        cfg.media.install_boot_order = None;
        let mut n = 0;

        for b in self.boot.to_vec().iter().filter(|b| b.enabled) {
            let slot = match b.kind {
                BootKind::Disk => disk_ids
                    .iter()
                    .position(|&id| id == b.id)
                    .map(|i| &mut cfg.disks[i].boot_order),
                BootKind::InstallDrive => Some(&mut cfg.media.install_boot_order),
                BootKind::Pci => pci_ids
                    .iter()
                    .position(|&id| id == b.id)
                    .map(|i| &mut cfg.pci[i].boot_order),
            };

            if let Some(slot) = slot {
                n += 1;
                *slot = Some(n);
            }
        }

        issues.extend(cfg.validate());

        Draft {
            cfg,
            issues,
            new_images,
        }
    }
}
