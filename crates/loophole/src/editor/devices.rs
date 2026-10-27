use loophole_vm::host::{self, Passthrough};
use slint::{ComponentHandle as _, ModelRc, VecModel};

use crate::ui::{Edit, HostDevices, HostDiskItem, HostPciItem, HostUsbItem};

use super::Editor;
use super::fields::{file_name, gib, text};

impl Editor {
    pub fn scan_host(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let devices = ui.global::<HostDevices>();
        let used: Vec<String> = self
            .disks
            .to_vec()
            .iter()
            .map(|d| d.path.to_string())
            .collect();

        devices.set_disks(ModelRc::new(VecModel::from(
            host::block_devices()
                .into_iter()
                .map(|b| {
                    let path = b.path.display().to_string();

                    let problem = if used.contains(&path) {
                        "attached to this VM already".to_owned()
                    } else if let Some(why) = &b.in_use {
                        format!("{why} on the host")
                    } else if !b.accessible {
                        format!(
                            "no access to {}: add it to programs.loophole.blockDevices",
                            b.node.display()
                        )
                    } else {
                        String::new()
                    };

                    HostDiskItem {
                        title: text(if b.model.is_empty() {
                            file_name(&b.node)
                        } else {
                            b.model.clone()
                        }),
                        detail: text(format!("{}  {}  {}", gib(b.size), b.node.display(), path)),
                        path: text(path),
                        problem: text(problem),
                    }
                })
                .collect::<Vec<_>>(),
        )));

        let attached: Vec<String> = self
            .pci
            .to_vec()
            .iter()
            .map(|p| p.address.to_string())
            .collect();

        devices.set_pci(ModelRc::new(VecModel::from(
            host::pci_devices()
                .into_iter()
                .map(|d| {
                    let status = host::passthrough(d.address);
                    let address = d.address.to_string();

                    let problem = if attached.contains(&address) {
                        "attached to this VM already".to_owned()
                    } else if status == Passthrough::Ready {
                        String::new()
                    } else {
                        status.describe()
                    };

                    let mut detail =
                        format!("{address}  {}", d.driver.as_deref().unwrap_or("no driver"));

                    if let Some(g) = d.iommu_group {
                        detail += &format!("  group {g}");
                    }

                    if !d.contents.is_empty() {
                        detail += &format!("  {}", d.contents.join(", "));
                    }

                    HostPciItem {
                        address: text(address),
                        title: text(d.name),
                        detail: text(detail),
                        problem: text(problem),
                    }
                })
                .collect::<Vec<_>>(),
        )));

        devices.set_usb(ModelRc::new(VecModel::from(
            host::usb_devices()
                .into_iter()
                .map(|u| HostUsbItem {
                    id: text(u.id.to_string()),
                    title: text(u.name),
                    problem: text(if u.accessible {
                        ""
                    } else {
                        "no access: add it to programs.loophole.usbDevices"
                    }),
                })
                .collect::<Vec<_>>(),
        )));

        let edit = ui.global::<Edit>();
        let taps = host::taps();
        let described: Vec<String> = taps
            .iter()
            .map(|t| {
                t.bridge
                    .as_ref()
                    .map_or_else(|| t.name.clone(), |br| format!("{} on {br}", t.name))
            })
            .collect();

        edit.set_taps_hint(text(if described.is_empty() {
            "None on this host: create one in your NixOS config".to_owned()
        } else {
            described.join("  ")
        }));

        edit.set_taps(ModelRc::new(VecModel::from(
            taps.into_iter().map(|t| text(t.name)).collect::<Vec<_>>(),
        )));
    }
}
