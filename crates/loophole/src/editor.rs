use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use loophole_vm::argv::{Host, RunPaths, argv};
use loophole_vm::config::{
    self, Capture, Disk, DiskBus, DiskCache, DiskFormat, DiskSource, Forward, HugePages, Issue,
    Mac, NetBackend, Nic, NicModel, PciAddress, PciPassthrough, Proto, Resolution, Section, UsbId,
    UsbPassthrough, VmConfig, Windows,
};
use loophole_vm::host::{self, Passthrough};
use loophole_vm::{Tools, dirs, run};
use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, VecModel};

use crate::models::Tracked;
use crate::ui::{
    BootEdit, DiskEdit, Edit, EditIssues, EditOptions, ForwardEdit, HostDevices, HostDiskItem,
    HostPciItem, HostUsbItem, MainWindow, NicEdit, PciEdit, UsbEdit, VmEdit,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Hidden,
    Create,
    Edit(String),
    View(String),
}

impl Mode {
    const fn code(&self) -> i32 {
        match self {
            Self::Hidden => 0,
            Self::Create => 1,
            Self::Edit(_) => 2,
            Self::View(_) => 3,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Edit(n) | Self::View(n) => Some(n),
            Self::Hidden | Self::Create => None,
        }
    }
}

const KIND_NEW: i32 = 0;
const KIND_IMAGE: i32 = 1;
const KIND_DEVICE: i32 = 2;

const BOOT_DISK: i32 = 0;
const BOOT_INSTALL: i32 = 1;
const BOOT_PCI: i32 = 2;

const NEW_DISK_GIB: i32 = 128;

pub struct Draft {
    pub cfg: VmConfig,
    pub issues: Vec<Issue>,
    pub new_images: Vec<(usize, u32)>,
}

impl Draft {
    pub fn fatal(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.fatal)
    }
}

fn text(s: impl Into<SharedString>) -> SharedString {
    s.into()
}

fn int(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

fn count(v: i32) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(0)
}

fn index(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(0)
}

fn optional(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

pub fn gib(bytes: u64) -> String {
    let g = bytes as f64 / f64::from(1_u32 << 30);
    if g >= 1000.0 {
        format!("{:.1} TiB", g / 1024.0)
    } else {
        format!("{g:.0} GiB")
    }
}

fn hugepage_pools() -> [Option<(&'static str, u64)>; 2] {
    [(HugePages::Size2M, "2 MiB"), (HugePages::Size1G, "1 GiB")].map(|(size, label)| {
        host::hugepages(size)
            .filter(|&(total, _)| total > 0)
            .map(|(_, free)| (label, free))
    })
}

fn hugepage_hints(pools: &[Option<(&str, u64)>; 2]) -> Vec<SharedString> {
    let hint = |p: &Option<(&str, u64)>| {
        p.map_or_else(
            || "None reserved".to_owned(),
            |(label, free)| format!("Available: {free} × {label}"),
        )
    };
    let all: Vec<String> = pools
        .iter()
        .flatten()
        .map(|(label, free)| format!("{free} × {label}"))
        .collect();
    vec![
        text(format!("Available: {}", all.join(", "))),
        text(hint(&pools[0])),
        text(hint(&pools[1])),
    ]
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(
        || p.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

pub const fn tab(section: Section) -> usize {
    match section {
        Section::General | Section::Cpu | Section::Memory | Section::Gpu => 0,
        Section::Disk(_) | Section::Media => 1,
        Section::Nic(_) => 2,
        Section::Pci(_) | Section::Usb(_) | Section::Sound => 3,
        Section::Display | Section::Input => 4,
        Section::Boot => 5,
        Section::Machine => 6,
    }
}

const TABS: usize = 7;

// A new VM's tabs: Setup, which holds its name, first disk and discs, then the settings tabs.
const fn create_tab(section: Section) -> usize {
    match section {
        Section::General | Section::Media | Section::Disk(0) => 0,
        s => tab(s) + 1,
    }
}

fn labels<T>(all: &[T], label: fn(T) -> &'static str) -> ModelRc<SharedString>
where
    T: Copy,
{
    ModelRc::new(VecModel::from(
        all.iter().map(|&v| text(label(v))).collect::<Vec<_>>(),
    ))
}

fn options() -> EditOptions {
    EditOptions {
        hugepages: labels(HugePages::ALL, |h| match h {
            HugePages::Off => "Off",
            HugePages::Size2M => "2 MiB pages",
            HugePages::Size1G => "1 GiB pages",
        }),
        buses: labels(DiskBus::ALL, |b| match b {
            DiskBus::Nvme => "NVMe",
            DiskBus::Virtio => "virtio-blk",
            DiskBus::Sata => "SATA",
        }),
        formats: labels(DiskFormat::ALL, |f| match f {
            DiskFormat::Auto => "By file name",
            DiskFormat::Qcow2 => "qcow2",
            DiskFormat::Raw => "Raw",
        }),
        caches: labels(DiskCache::ALL, |c| match c {
            DiskCache::Auto => "Automatic",
            DiskCache::Direct => "Direct I/O",
            DiskCache::Writeback => "Host cache (writeback)",
        }),
        nic_models: labels(NicModel::ALL, |m| match m {
            NicModel::E1000e => "Intel e1000e",
            NicModel::Virtio => "virtio-net",
        }),
        backends: labels(NetBackend::ALL, |b| match b {
            NetBackend::User => "NAT",
            NetBackend::Tap => "TAP device",
        }),
        protos: labels(Proto::ALL, |p| match p {
            Proto::Tcp => "TCP",
            Proto::Udp => "UDP",
        }),
        captures: labels(Capture::ALL, |c| match c {
            Capture::Auto => "Automatic",
            Capture::Manual => "Ctrl+Alt+G only",
            Capture::Never => "Never",
        }),
        windows: labels(Windows::ALL, Windows::as_str),
    }
}

fn disk_info(d: &DiskEdit) -> (String, String) {
    let path = Path::new(d.path.as_str());
    match d.kind {
        KIND_NEW => (
            "New disk".into(),
            format!("{} GiB qcow2, created on apply", d.new_gib),
        ),
        KIND_DEVICE => {
            let dev = host::block_device(path);
            let title = dev
                .as_ref()
                .map(|b| b.model.clone())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| file_name(path));
            let info = dev.map_or_else(
                || "host disk".into(),
                |b| format!("host disk · {} · {}", gib(b.size), b.node.display()),
            );
            (title, info)
        },
        _ => {
            let size = std::fs::metadata(path).map_or(String::new(), |m| {
                format!(" · {} on the host", gib(m.len()))
            });
            (
                if d.path.is_empty() {
                    "Image".into()
                } else {
                    file_name(path)
                },
                format!("image{size}"),
            )
        },
    }
}

fn quote(arg: &OsString) -> String {
    let s = arg.to_string_lossy();
    if s.is_empty() || s.contains([' ', '"', '\'']) {
        format!("'{}'", s.replace('\'', r"'\''"))
    } else {
        s.into_owned()
    }
}

pub struct Editor {
    ui: slint::Weak<MainWindow>,
    pub mode: Mode,
    saved: Option<VmConfig>,
    disks: Rc<Tracked<DiskEdit>>,
    nics: Rc<Tracked<NicEdit>>,
    pci: Rc<Tracked<PciEdit>>,
    usb: Rc<VecModel<UsbEdit>>,
    boot: Rc<Tracked<BootEdit>>,
    on_edit: Rc<dyn Fn()>,
    next_id: i32,
    pub generation: u64,
    pub pick_disk: Option<usize>,
    pub dirty: bool,
}

impl Editor {
    pub fn new(ui: &MainWindow, on_edit: Rc<dyn Fn()>) -> Self {
        let this = Self {
            ui: ui.as_weak(),
            mode: Mode::Hidden,
            saved: None,
            disks: Tracked::new(on_edit.clone()),
            nics: Tracked::new(on_edit.clone()),
            pci: Tracked::new(on_edit.clone()),
            usb: Rc::new(VecModel::default()),
            boot: Tracked::new(on_edit.clone()),
            on_edit,
            next_id: 1,
            generation: 0,
            pick_disk: None,
            dirty: false,
        };
        let edit = ui.global::<Edit>();
        edit.set_disks(this.disks.model());
        edit.set_nics(this.nics.model());
        edit.set_pci(this.pci.model());
        edit.set_usb(ModelRc::from(this.usb.clone()));
        edit.set_boot(this.boot.model());
        edit.set_options(options());
        edit.set_tab_issues(ModelRc::new(VecModel::from(vec![0; TABS])));
        edit.set_host_cpus(text(format!("Available: {} threads", host::cpu_count())));
        edit.set_host_memory(text(format!("Available: {}", gib(host::memory_total()))));
        let pools = hugepage_pools();
        edit.set_hugepages_available(pools.iter().any(Option::is_some));
        edit.set_host_hugepages(ModelRc::new(VecModel::from(hugepage_hints(&pools))));
        this
    }

    fn id(&mut self) -> i32 {
        self.next_id += 1;
        self.next_id
    }

    fn tab_of(&self, section: Section) -> usize {
        if self.mode == Mode::Create {
            create_tab(section)
        } else {
            tab(section)
        }
    }

    fn with_edit(&self, f: impl FnOnce(&Edit<'_>)) {
        if let Some(ui) = self.ui.upgrade() {
            f(&ui.global::<Edit>());
        }
    }

    pub fn hide(&mut self) {
        self.mode = Mode::Hidden;
        self.saved = None;
        self.dirty = false;
        self.generation += 1;
        self.with_edit(|e| {
            e.set_mode(0);
            e.set_dirty(false);
            e.set_summary(SharedString::new());
        });
    }

    pub fn load(&mut self, mode: Mode, cfg: &VmConfig) {
        let keep_tab = self.mode.name() == mode.name();
        self.mode = mode;
        self.saved = Some(cfg.clone());
        self.dirty = false;
        self.generation += 1;
        self.show(cfg);
        let code = self.mode.code();
        self.with_edit(|e| {
            e.set_mode(code);
            if !keep_tab {
                e.set_tab(0);
            }
        });
    }

    pub fn create(&mut self) {
        self.mode = Mode::Create;
        self.saved = None;
        self.generation += 1;
        let mut cfg = VmConfig::new("");
        cfg.media.driver_disc = true;
        self.show(&cfg);
        self.disks.update(0, |d| d.kind = KIND_NEW);
        self.refresh_disk_titles();
        self.with_edit(|e| {
            e.set_mode(1);
            e.set_tab(0);
        });
    }

    pub fn revert(&mut self) {
        if let Some(cfg) = self.saved.clone() {
            self.generation += 1;
            self.show(&cfg);
        }
    }

    fn show(&mut self, cfg: &VmConfig) {
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
            gpu_window_gib: int(cfg.gpu.window_gib),
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
            tpm: cfg.machine.tpm,
            machine_options: text(cfg.machine.options.as_str()),
            extra_args: text(cfg.machine.extra_args.join(" ")),
        });

        let disks: Vec<DiskEdit> = cfg
            .disks
            .iter()
            .map(|d| {
                let (kind, path) = match &d.source {
                    DiskSource::Image(p) => (KIND_IMAGE, p),
                    DiskSource::Device(p) => (KIND_DEVICE, p),
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
                kind: BOOT_DISK,
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
            kind: BOOT_INSTALL,
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
                kind: BOOT_PCI,
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

    fn nic_edit(&self, n: &Nic) -> NicEdit {
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

    fn refresh_disk_titles(&self) {
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

    fn refresh_pci(&self) {
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

    fn refresh_boot_labels(&self) {
        let disks = self.disks.to_vec();
        let pci = self.pci.to_vec();
        let install = self
            .ui
            .upgrade()
            .map(|ui| ui.global::<Edit>().get_vm().install)
            .unwrap_or_default();
        for (i, b) in self.boot.to_vec().into_iter().enumerate() {
            let (label, detail) = match b.kind {
                BOOT_DISK => disks.iter().position(|d| d.id == b.id).map_or(
                    (String::new(), String::new()),
                    |n| {
                        (
                            format!("Disk {}: {}", n + 1, disks[n].title),
                            disks[n].info.to_string(),
                        )
                    },
                ),
                BOOT_INSTALL => (
                    "Windows installation drive".into(),
                    if install.is_empty() {
                        "empty".into()
                    } else {
                        file_name(Path::new(install.as_str()))
                    },
                ),
                _ => pci
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

    // Boot rows follow the disks and PCI devices: new ones go last, unticked.
    fn sync_boot(&self) {
        let disks = self.disks.to_vec();
        let pci = self.pci.to_vec();
        let mut rows: Vec<BootEdit> = self
            .boot
            .to_vec()
            .into_iter()
            .filter(|b| match b.kind {
                BOOT_DISK => disks.iter().any(|d| d.id == b.id),
                BOOT_PCI => pci.iter().any(|p| p.id == b.id),
                _ => true,
            })
            .collect();
        for d in &disks {
            if !rows.iter().any(|b| b.kind == BOOT_DISK && b.id == d.id) {
                rows.push(BootEdit {
                    kind: BOOT_DISK,
                    id: d.id,
                    enabled: !rows.iter().any(|b| b.enabled),
                    ..BootEdit::default()
                });
            }
        }
        for p in &pci {
            if !rows.iter().any(|b| b.kind == BOOT_PCI && b.id == p.id) {
                rows.push(BootEdit {
                    kind: BOOT_PCI,
                    id: p.id,
                    enabled: false,
                    ..BootEdit::default()
                });
            }
        }
        self.boot.rows().set_vec(rows);
        self.refresh_boot_labels();
    }

    pub fn add_disk(&mut self, kind: i32, path: Option<PathBuf>) {
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

    pub fn set_disk(&self, i: usize, kind: i32, path: &Path) {
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
        let mut rows = self.boot.to_vec();
        let Some(j) = i.checked_add_signed(delta as isize) else {
            return;
        };
        if i < rows.len() && j < rows.len() {
            rows.swap(i, j);
            self.boot.rows().set_vec(rows);
            (self.on_edit)();
        }
    }

    // A lenient reading of the form: what does not parse becomes an issue, not a failure.
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
        cfg.gpu.window_gib = count(vm.gpu_window_gib);
        cfg.media.install = optional(&vm.install).map(PathBuf::from);
        cfg.media.driver_disc = vm.driver_disc;
        cfg.sound.enabled = vm.sound;
        cfg.sound.microphone = vm.microphone;
        cfg.input.tablet = vm.tablet;
        cfg.boot.menu = vm.boot_menu;
        cfg.boot.menu_timeout_ms = count(vm.boot_timeout);
        cfg.display.capture = Capture::from_index(count(vm.capture) as usize);
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
                KIND_NEW => {
                    let p = cfg.free_image_path(&planned);
                    planned.push(p.clone());
                    new_images.push((i, count(d.new_gib)));
                    DiskSource::Image(p)
                },
                KIND_DEVICE => DiskSource::Device(path),
                _ => DiskSource::Image(path),
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
                BOOT_DISK => disk_ids
                    .iter()
                    .position(|&id| id == b.id)
                    .map(|i| &mut cfg.disks[i].boot_order),
                BOOT_INSTALL => Some(&mut cfg.media.install_boot_order),
                BOOT_PCI => pci_ids
                    .iter()
                    .position(|&id| id == b.id)
                    .map(|i| &mut cfg.pci[i].boot_order),
                _ => None,
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

    pub fn refresh(&mut self, others: &[String], tools: Option<&Tools>) {
        if self.mode == Mode::Hidden {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        self.refresh_disk_titles();
        self.refresh_boot_labels();
        let draft = self.draft(others);
        let preflight: Vec<Issue> = run::preflight(&draft.cfg)
            .into_iter()
            .filter(|i| {
                !matches!(i.section, Section::Disk(d) if draft.new_images.iter().any(|(n, _)| *n == d))
            })
            .collect();
        self.dirty = match &self.mode {
            Mode::Create => true,
            _ => self.saved.as_ref() != Some(&draft.cfg) || !draft.new_images.is_empty(),
        };

        let mut by_section: BTreeMap<Section, Vec<String>> = BTreeMap::new();
        let mut warnings_by: BTreeMap<Section, Vec<String>> = BTreeMap::new();
        let mut tabs = vec![0; TABS + usize::from(self.mode == Mode::Create)];
        for i in &draft.issues {
            if i.fatal {
                by_section
                    .entry(i.section)
                    .or_default()
                    .push(i.message.clone());
                tabs[self.tab_of(i.section)] += 1;
            } else {
                warnings_by
                    .entry(i.section)
                    .or_default()
                    .push(i.message.clone());
            }
        }
        for i in &preflight {
            warnings_by
                .entry(i.section)
                .or_default()
                .push(i.message.clone());
        }
        let join = |m: &BTreeMap<Section, Vec<String>>, s: Section| {
            text(m.get(&s).map(|v| v.join(" · ")).unwrap_or_default())
        };
        let edit = ui.global::<Edit>();
        edit.set_issues(EditIssues {
            general: join(&by_section, Section::General),
            cpu: join(&by_section, Section::Cpu),
            memory: join(&by_section, Section::Memory),
            gpu: join(&by_section, Section::Gpu),
            media: join(&by_section, Section::Media),
            sound: join(&by_section, Section::Sound),
            input: join(&by_section, Section::Input),
            boot: join(&by_section, Section::Boot),
            display: join(&by_section, Section::Display),
            machine: join(&by_section, Section::Machine),
        });
        for (i, d) in self.disks.to_vec().into_iter().enumerate() {
            let error = join(&by_section, Section::Disk(i));
            let warning = join(&warnings_by, Section::Disk(i));
            if d.error != error || d.warning != warning {
                self.disks.rows().set_row_data(
                    i,
                    DiskEdit {
                        error,
                        warning,
                        ..d
                    },
                );
            }
        }
        for (i, n) in self.nics.to_vec().into_iter().enumerate() {
            let error = join(&by_section, Section::Nic(i));
            if n.error != error {
                self.nics.rows().set_row_data(i, NicEdit { error, ..n });
            }
        }
        self.refresh_pci();
        let present = host::usb_devices();
        for (i, u) in self.usb.iter().enumerate() {
            let status = text(match present.iter().find(|d| u.id == d.id.to_string()) {
                None => "not plugged in",
                Some(d) if !d.accessible => "no access",
                Some(_) => "",
            });
            if u.status != status {
                self.usb.set_row_data(i, UsbEdit { status, ..u });
            }
        }

        let fatal: Vec<&Issue> = draft.fatal().collect();
        let summary = match fatal.as_slice() {
            [] => String::new(),
            [one] => one.message.clone(),
            [first, rest @ ..] => format!("{} · {} more", first.message, rest.len()),
        };
        let warnings: Vec<SharedString> = draft
            .issues
            .iter()
            .filter(|i| !i.fatal)
            .chain(preflight.iter())
            .map(|i| text(i.message.as_str()))
            .collect();
        edit.set_summary(text(summary));
        edit.set_warnings(ModelRc::new(VecModel::from(warnings)));
        edit.set_tab_issues(ModelRc::new(VecModel::from(tabs)));
        edit.set_dirty(self.dirty);
        edit.set_install_name(text(
            draft
                .cfg
                .media
                .install
                .as_deref()
                .map(file_name)
                .unwrap_or_default(),
        ));
        edit.set_preview(text(
            tools.map(|t| preview(&draft.cfg, t)).unwrap_or_default(),
        ));
    }

    // The first tab with a problem, when Apply cannot go ahead.
    pub fn show_problem(&self, draft: &Draft) {
        if let Some(first) = draft.fatal().next() {
            let t = self.tab_of(first.section);
            self.with_edit(|e| e.set_tab(index(t)));
        }
    }

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
                        detail: text(format!("{} · {} · {}", gib(b.size), b.node.display(), path)),
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
                        format!("{address} · {}", d.driver.as_deref().unwrap_or("no driver"));
                    if let Some(g) = d.iommu_group {
                        detail += &format!(" · group {g}");
                    }
                    if !d.contents.is_empty() {
                        detail += &format!(" · {}", d.contents.join(", "));
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
            described.join(" · ")
        }));
        edit.set_taps(ModelRc::new(VecModel::from(
            taps.into_iter().map(|t| text(t.name)).collect::<Vec<_>>(),
        )));
    }
}

fn preview(cfg: &VmConfig, tools: &Tools) -> String {
    let nodes = host::render_nodes();
    let render =
        host::gpu(&nodes).map_or_else(|| PathBuf::from("/dev/dri/renderD128"), |n| n.path.clone());
    let block_sizes: Vec<Option<u32>> = cfg
        .disks
        .iter()
        .map(|d| match &d.source {
            DiskSource::Device(p) => host::block_device(p).map(|b| b.logical_block),
            DiskSource::Image(_) => None,
        })
        .collect();
    let host = Host {
        render_node: &render,
        pipewire: host::pipewire(),
        io_uring: host::io_uring(),
        block_sizes: &block_sizes,
    };
    let vars = loophole_vm::dirs::data_dir(&cfg.name).join("OVMF_VARS.fd");
    argv(cfg, tools, &RunPaths::new(&cfg.name), &vars, &host)
        .iter()
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ")
}
