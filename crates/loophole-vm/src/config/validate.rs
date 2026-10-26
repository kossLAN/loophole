use std::collections::BTreeMap;
use std::fmt;

use super::{DiskBus, DiskFormat, DiskSource, NetBackend, VmConfig};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    General,
    Cpu,
    Memory,
    Gpu,
    Media,
    Disk(usize),
    Nic(usize),
    Pci(usize),
    Usb(usize),
    Sound,
    Input,
    Boot,
    Display,
    Machine,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub section: Section,
    pub message: String,
    pub fatal: bool,
}

impl Issue {
    pub fn error(section: Section, message: impl Into<String>) -> Self {
        Self {
            section,
            message: message.into(),
            fatal: true,
        }
    }

    pub fn warning(section: Section, message: impl Into<String>) -> Self {
        Self {
            section,
            message: message.into(),
            fatal: false,
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub const MAX_DISKS: usize = 7;
pub const MAX_SATA: usize = 4;
pub const MAX_NICS: usize = 5;
pub const MAX_PCI: usize = 8;
const MAX_SERIAL: usize = 20;

impl VmConfig {
    pub fn validate(&self) -> Vec<Issue> {
        let mut out = Vec::new();
        let mut err = |s, m: String| out.push(Issue::error(s, m));

        if !(1..=2).contains(&self.cpu.sockets) {
            err(
                Section::Cpu,
                "sockets: 1 or 2, as Windows client editions allow".into(),
            );
        }
        if !(1..=256).contains(&self.cpu.cores) {
            err(Section::Cpu, "cores: 1 to 256".into());
        }
        if !(1..=2).contains(&self.cpu.threads) {
            err(Section::Cpu, "threads per core: 1 or 2".into());
        }
        if self.memory.size_gib == 0 {
            err(Section::Memory, "memory: at least 1 GiB".into());
        }
        if !(1..=64).contains(&self.gpu.window_gib) || !self.gpu.window_gib.is_power_of_two() {
            err(
                Section::Gpu,
                "GPU memory window: a power of two from 1 to 64 GiB".into(),
            );
        }

        if self.disks.len() > MAX_DISKS {
            err(
                Section::Disk(MAX_DISKS),
                format!("at most {MAX_DISKS} disks"),
            );
        }
        let sata = self.disks.iter().filter(|d| d.bus == DiskBus::Sata).count();
        if sata > MAX_SATA {
            err(Section::Disk(0), format!("at most {MAX_SATA} SATA disks"));
        }
        let mut serials = BTreeMap::new();
        for (i, d) in self.disks.iter().enumerate() {
            if d.source.path().as_os_str().is_empty() {
                err(Section::Disk(i), "choose an image or a device".into());
            }
            if matches!(d.source, DiskSource::Device(_)) && d.format == DiskFormat::Qcow2 {
                err(Section::Disk(i), "a host device is raw".into());
            }
            if let Some(s) = &d.serial {
                if s.len() > MAX_SERIAL || !s.is_ascii() || s.contains(',') {
                    err(
                        Section::Disk(i),
                        format!("serial: up to {MAX_SERIAL} ASCII characters, no commas"),
                    );
                }
                if let Some(first) = serials.insert(s.clone(), i) {
                    err(
                        Section::Disk(i),
                        format!("serial {s} is disk {}'s already", first + 1),
                    );
                }
            }
        }

        let mut boot = BTreeMap::new();
        let mut order = |section: Section, n: Option<u32>, out: &mut Vec<Issue>| {
            let Some(n) = n else { return };
            if n == 0 {
                out.push(Issue::error(section, "boot order starts at 1"));
            } else if boot.insert(n, section).is_some() {
                out.push(Issue::error(section, format!("boot order {n} is taken")));
            }
        };
        for (i, d) in self.disks.iter().enumerate() {
            order(Section::Disk(i), d.boot_order, &mut out);
        }
        order(Section::Media, self.media.install_boot_order, &mut out);
        for (i, p) in self.pci.iter().enumerate() {
            order(Section::Pci(i), p.boot_order, &mut out);
        }

        let mut err = |s, m: String| out.push(Issue::error(s, m));
        if self.nics.len() > MAX_NICS {
            err(
                Section::Nic(MAX_NICS),
                format!("at most {MAX_NICS} network adapters"),
            );
        }
        let mut macs = BTreeMap::new();
        let mut ports = BTreeMap::new();
        for (i, n) in self.nics.iter().enumerate() {
            match n.backend {
                NetBackend::Tap => {
                    if n.tap.as_deref().is_none_or(str::is_empty) {
                        err(Section::Nic(i), "choose a TAP device".into());
                    }
                    if !n.forwards.is_empty() {
                        err(Section::Nic(i), "port forwarding needs NAT".into());
                    }
                },
                NetBackend::User => {},
            }
            if let Some(mac) = n.mac
                && macs.insert(mac, i).is_some()
            {
                err(Section::Nic(i), format!("MAC {mac} is used twice"));
            }
            for f in &n.forwards {
                if f.host == 0 || f.guest == 0 {
                    err(Section::Nic(i), "ports are 1 to 65535".into());
                }
                if ports.insert((f.proto, f.address, f.host), i).is_some() {
                    err(
                        Section::Nic(i),
                        format!("host port {}/{} is forwarded twice", f.host, f.proto),
                    );
                }
            }
        }

        if self.pci.len() > MAX_PCI {
            err(
                Section::Pci(MAX_PCI),
                format!("at most {MAX_PCI} PCI devices"),
            );
        }
        let mut seen = BTreeMap::new();
        for (i, p) in self.pci.iter().enumerate() {
            if seen.insert(p.address, i).is_some() {
                err(Section::Pci(i), format!("{} is listed twice", p.address));
            }
        }
        let mut seen = BTreeMap::new();
        for (i, u) in self.usb.iter().enumerate() {
            if seen.insert(u.id, i).is_some() {
                err(Section::Usb(i), format!("{} is listed twice", u.id));
            }
        }
        if let Some(hz) = self.display.refresh_hz
            && !(1..=1000).contains(&hz)
        {
            err(Section::Display, "refresh rate: 1 to 1000 Hz".into());
        }

        let bootable = self.disks.iter().any(|d| d.boot_order.is_some())
            || self.media.install_boot_order.is_some() && self.media.install.is_some()
            || self.pci.iter().any(|p| p.boot_order.is_some());
        if !bootable {
            out.push(Issue::warning(Section::Boot, "nothing is set to boot"));
        }
        if !self.input.tablet && self.display.capture == super::Capture::Never {
            out.push(Issue::warning(
                Section::Input,
                "without the tablet the pointer only works captured",
            ));
        }
        out
    }
}
