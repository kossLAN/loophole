use std::path::{Path, PathBuf};

use loophole_vm::config::{DiskBus, DiskCache, DiskFormat, Mac, Nic, PciAddress};
use loophole_vm::host::{self, Passthrough};
use slint::{ComponentHandle as _, Model as _, SharedString};

use crate::models::{self, Tracked};
use crate::ui::{BootEdit, BootKind, DiskEdit, DiskKind, Edit, ForwardEdit, PciEdit, UsbEdit};

use super::fields::{disk_info, file_name, index, text};
use super::{Editor, NEW_DISK_GIB};

impl Editor {
    pub(super) fn refresh_disk_titles(&self) {
        for (i, d) in self.disks.to_vec().into_iter().enumerate() {
            let (title, info) = disk_info(&d);

            if d.title != title.as_str() || d.info != info.as_str() {
                self.disks.rows().set_row_data(
                    i,
                    DiskEdit {
                        title: text(title),
                        info: text(info),
                        ..d
                    },
                );
            }
        }
    }

    pub(super) fn refresh_pci(&self) {
        let devices = host::pci_devices();

        for (i, p) in self.pci.to_vec().into_iter().enumerate() {
            let address: Option<PciAddress> = p.address.parse().ok();
            let (name, status) = address.map_or_else(
                || ("PCI device".to_owned(), Passthrough::Missing),
                |a| {
                    (
                        devices
                            .iter()
                            .find(|d| d.address == a)
                            .map_or_else(|| a.to_string(), |d| d.name.clone()),
                        host::passthrough(a),
                    )
                },
            );
            let row = PciEdit {
                name: text(name),
                ready: status == Passthrough::Ready,
                status: text(status.describe()),
                ..p.clone()
            };

            if row != p {
                self.pci.rows().set_row_data(i, row);
            }
        }
    }

    pub(super) fn refresh_boot_labels(&self) {
        let disks = self.disks.to_vec();
        let pci = self.pci.to_vec();
        let install = self
            .ui
            .upgrade()
            .map(|ui| ui.global::<Edit>().get_vm().install)
            .unwrap_or_default();

        for (i, b) in self.boot.to_vec().into_iter().enumerate() {
            let (label, detail) = match b.kind {
                BootKind::Disk => disks.iter().position(|d| d.id == b.id).map_or(
                    (String::new(), String::new()),
                    |n| {
                        (
                            format!("Disk {}: {}", n + 1, disks[n].title),
                            disks[n].info.to_string(),
                        )
                    },
                ),
                BootKind::InstallDrive => (
                    "Windows installation drive".into(),
                    if install.is_empty() {
                        "empty".into()
                    } else {
                        file_name(Path::new(install.as_str()))
                    },
                ),
                BootKind::Pci => pci
                    .iter()
                    .find(|p| p.id == b.id)
                    .map_or((String::new(), String::new()), |p| {
                        (p.name.to_string(), p.address.to_string())
                    }),
            };

            if b.label != label.as_str() || b.detail != detail.as_str() {
                self.boot.rows().set_row_data(
                    i,
                    BootEdit {
                        label: text(label),
                        detail: text(detail),
                        ..b
                    },
                );
            }
        }
    }

    pub(super) fn sync_boot(&self) {
        let disks = self.disks.to_vec();
        let pci = self.pci.to_vec();
        let mut rows: Vec<BootEdit> = self
            .boot
            .to_vec()
            .into_iter()
            .filter(|b| match b.kind {
                BootKind::Disk => disks.iter().any(|d| d.id == b.id),
                BootKind::Pci => pci.iter().any(|p| p.id == b.id),
                BootKind::InstallDrive => true,
            })
            .collect();

        for d in &disks {
            if !rows
                .iter()
                .any(|b| b.kind == BootKind::Disk && b.id == d.id)
            {
                rows.push(BootEdit {
                    kind: BootKind::Disk,
                    id: d.id,
                    enabled: !rows.iter().any(|b| b.enabled),
                    ..BootEdit::default()
                });
            }
        }

        for p in &pci {
            if !rows.iter().any(|b| b.kind == BootKind::Pci && b.id == p.id) {
                rows.push(BootEdit {
                    kind: BootKind::Pci,
                    id: p.id,
                    enabled: false,
                    ..BootEdit::default()
                });
            }
        }

        models::sync(self.boot.rows(), rows);
        self.refresh_boot_labels();
    }

    pub fn add_disk(&mut self, kind: DiskKind, path: Option<PathBuf>) {
        let taken: Vec<String> = self
            .disks
            .to_vec()
            .iter()
            .map(|d| d.serial.to_string())
            .collect();
        let serial = (0..64)
            .map(|i| format!("loophole{i}"))
            .find(|s| !taken.contains(s))
            .unwrap_or_default();
        let id = self.id();
        let d = DiskEdit {
            id,
            kind,
            path: path
                .map(|p| text(p.display().to_string()))
                .unwrap_or_default(),
            new_gib: NEW_DISK_GIB,
            bus: index(DiskBus::Nvme.index()),
            format: index(DiskFormat::Auto.index()),
            cache: index(DiskCache::Auto.index()),
            discard: true,
            serial: text(serial),
            ..DiskEdit::default()
        };
        self.disks.rows().push(d);
        self.refresh_disk_titles();
        self.sync_boot();
        (self.on_edit)();
    }

    pub fn remove_disk(&self, i: usize) {
        if i < self.disks.row_count() {
            self.disks.rows().remove(i);
            self.sync_boot();
            (self.on_edit)();
        }
    }

    pub fn set_disk(&self, i: usize, kind: DiskKind, path: &Path) {
        self.disks.update(i, |d| {
            d.kind = kind;
            d.path = text(path.display().to_string());
        });
        self.refresh_disk_titles();
        self.refresh_boot_labels();
        (self.on_edit)();
    }

    pub fn set_install(&self, path: &Path) {
        self.with_edit(|e| {
            let mut vm = e.get_vm();
            vm.install = text(path.display().to_string());
            e.set_vm(vm);
        });
    }

    pub fn add_nic(&self) {
        let nic = Nic {
            mac: Some(Mac::random()),
            ..Nic::default()
        };
        self.nics.rows().push(self.nic_edit(&nic));
        (self.on_edit)();
    }

    pub fn remove_nic(&self, i: usize) {
        if i < self.nics.row_count() {
            self.nics.rows().remove(i);
            (self.on_edit)();
        }
    }

    pub fn new_mac(&self, i: usize) {
        self.nics
            .update(i, |n| n.mac = text(Mac::random().to_string()));
        (self.on_edit)();
    }

    pub fn add_forward(&self, i: usize) {
        if let Some(n) = self.nics.row_data(i)
            && let Some(f) = Tracked::from_model(&n.forwards)
        {
            f.rows().push(ForwardEdit {
                proto: 0,
                address: text("127.0.0.1"),
                host: 3389,
                guest: 3389,
            });
            (self.on_edit)();
        }
    }

    pub fn remove_forward(&self, i: usize, j: usize) {
        if let Some(n) = self.nics.row_data(i)
            && let Some(f) = Tracked::from_model(&n.forwards)
            && j < f.row_count()
        {
            f.rows().remove(j);
            (self.on_edit)();
        }
    }

    pub fn add_pci(&mut self, address: &str) {
        if self.pci.to_vec().iter().any(|p| p.address == address) {
            return;
        }

        let id = self.id();
        self.pci.rows().push(PciEdit {
            id,
            address: text(address),
            rom_bar: true,
            ..PciEdit::default()
        });
        self.refresh_pci();
        self.sync_boot();
        (self.on_edit)();
    }

    pub fn remove_pci(&self, i: usize) {
        if i < self.pci.row_count() {
            self.pci.rows().remove(i);
            self.sync_boot();
            (self.on_edit)();
        }
    }

    pub fn add_usb(&self, id: &str) {
        if self.usb.iter().any(|u| u.id == id) {
            return;
        }

        let name = host::usb_devices()
            .into_iter()
            .find(|d| d.id.to_string() == id)
            .map_or_else(|| id.to_owned(), |d| d.name);
        self.usb.push(UsbEdit {
            id: text(id),
            name: text(name),
            status: SharedString::new(),
        });
        (self.on_edit)();
    }

    pub fn remove_usb(&self, i: usize) {
        if i < self.usb.row_count() {
            self.usb.remove(i);
            (self.on_edit)();
        }
    }

    pub fn move_boot(&self, i: usize, delta: i32) {
        let Some(j) = i.checked_add_signed(delta as isize) else {
            return;
        };

        let rows = self.boot.rows();

        if let (Some(a), Some(b)) = (rows.row_data(i), rows.row_data(j)) {
            rows.set_row_data(i, b);
            rows.set_row_data(j, a);
            (self.on_edit)();
        }
    }
}
