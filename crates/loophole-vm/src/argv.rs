use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::{DiskBus, DiskFormat, DiskSource, HugePages, NetBackend, NicModel, VmConfig};
use crate::dirs;
use crate::tools::Tools;

#[derive(Clone, Debug)]
pub struct RunPaths {
    pub dir: PathBuf,
}

impl RunPaths {
    pub fn new(name: &str) -> Self {
        Self {
            dir: dirs::runtime_dir(name),
        }
    }

    // Each QMP socket serves one client: this one stays with the manager while the VM runs.
    pub fn control(&self) -> PathBuf {
        self.dir.join("control.sock")
    }

    pub fn qmp(&self) -> PathBuf {
        self.dir.join("qmp.sock")
    }

    pub fn tpm(&self) -> PathBuf {
        self.dir.join("swtpm.sock")
    }

    pub fn tpm_pid(&self) -> PathBuf {
        self.dir.join("swtpm.pid")
    }

    pub fn pid(&self) -> PathBuf {
        self.dir.join("qemu.pid")
    }

    pub fn log(&self) -> PathBuf {
        self.dir.join("qemu.log")
    }

    pub fn tpm_log(&self) -> PathBuf {
        self.dir.join("swtpm.log")
    }

    pub fn serial(&self) -> PathBuf {
        self.dir.join("serial.log")
    }
}

pub const INSTALL_DRIVE: &str = "install";
pub const DRIVER_DRIVE: &str = "driver";

pub struct Host<'a> {
    pub render_node: &'a Path,
    pub pipewire: bool,
    pub io_uring: bool,
    pub block_sizes: &'a [Option<u32>],
}

mod slot {
    pub const NIC0: u32 = 0x01;
    pub const DISK0: u32 = 0x02;
    pub const XHCI: u32 = 0x03;
    pub const GPU: u32 = 0x04;
    pub const HDA: u32 = 0x05;
    pub const DISKS: u32 = 0x05;
    pub const NICS: u32 = 0x0b;
    pub const ROOT_PORTS: u32 = 0x10;
}

const fn disk_slot(i: usize) -> u32 {
    if i == 0 {
        slot::DISK0
    } else {
        slot::DISKS + i as u32
    }
}

const fn nic_slot(i: usize) -> u32 {
    if i == 0 {
        slot::NIC0
    } else {
        slot::NICS + i as u32
    }
}

fn escaped(value: &str) -> String {
    value.replace(',', ",,")
}

fn escaped_path(path: &Path) -> OsString {
    match path.to_str() {
        Some(text) if text.contains(',') => escaped(text).into(),
        _ => path.into(),
    }
}

fn option(prefix: &str, path: &Path, suffix: &str) -> OsString {
    let mut s = OsString::from(prefix);
    s.push(escaped_path(path));
    s.push(suffix);
    s
}

fn bootindex(n: Option<u32>) -> String {
    n.map(|n| format!(",bootindex={n}")).unwrap_or_default()
}

struct Argv(Vec<OsString>);

impl Argv {
    fn flag(&mut self, flag: &str) {
        self.0.push(flag.into());
    }

    fn pair(&mut self, flag: &str, value: impl Into<OsString>) {
        self.0.push(flag.into());
        self.0.push(value.into());
    }
}

const HYPERV: &str = "hv_relaxed,hv_vapic,hv_spinlocks=0x1fff,hv_time,hv_vpindex,hv_synic,\
                      hv_stimer,hv_frequencies,hv_tlbflush,hv_ipi";

pub fn argv(
    cfg: &VmConfig,
    tools: &Tools,
    run: &RunPaths,
    vars: &Path,
    host: &Host<'_>,
) -> Vec<OsString> {
    let mut a = Argv(vec![tools.qemu.clone().into()]);
    let memory = format!("{}G", cfg.memory.size_gib);
    a.pair("-name", cfg.name.as_str());
    a.flag("-enable-kvm");
    let mut machine = "q35,smm=on,memory-backend=mem".to_owned();

    if !cfg.machine.options.trim().is_empty() {
        machine = format!("{machine},{}", cfg.machine.options.trim());
    }

    a.pair("-machine", machine);
    a.pair("-global", "driver=cfi.pflash01,property=secure,value=on");
    a.pair("-global", "ICH9-LPC.disable_s3=1");
    a.pair("-global", "ICH9-LPC.disable_s4=1");
    a.pair(
        "-cpu",
        if cfg.cpu.hyperv {
            format!("host,{HYPERV},-svm,-vmx")
        } else {
            "host,-svm,-vmx".into()
        },
    );
    a.pair(
        "-smp",
        format!(
            "sockets={},cores={},threads={}",
            cfg.cpu.sockets, cfg.cpu.cores, cfg.cpu.threads
        ),
    );
    a.pair("-m", memory.as_str());
    let mut backend = format!("memory-backend-memfd,id=mem,size={memory},share=on");

    match cfg.memory.hugepages {
        HugePages::Off => {},
        HugePages::Size2M => backend += ",hugetlb=on,hugetlbsize=2M,prealloc=on",
        HugePages::Size1G => backend += ",hugetlb=on,hugetlbsize=1G,prealloc=on",
    }

    a.pair("-object", backend);
    a.pair(
        "-drive",
        option(
            "if=pflash,format=raw,unit=0,readonly=on,file=",
            &tools.ovmf_code,
            "",
        ),
    );
    a.pair(
        "-drive",
        option("if=pflash,format=raw,unit=1,file=", vars, ""),
    );

    if cfg.machine.tpm {
        a.pair("-chardev", option("socket,id=tpm,path=", &run.tpm(), ""));
        a.pair("-tpmdev", "emulator,id=tpm0,chardev=tpm");
        a.pair("-device", "tpm-crb,tpmdev=tpm0");
    }

    let mut sata = 2;

    for (i, d) in cfg.disks.iter().enumerate() {
        let (driver, path) = match &d.source {
            DiskSource::Image(p) => ("file", p),
            DiskSource::Device(p) => ("host_device", p),
        };

        let aio = match (d.direct(), host.io_uring) {
            (false, _) => "threads",
            (true, true) => "io_uring",
            (true, false) => "native",
        };

        let discard = if d.discard { ",discard=unmap" } else { "" };

        let direct = if d.direct() { "on" } else { "off" };
        a.pair(
            "-blockdev",
            option(
                &format!("driver={driver},node-name=disk{i}-file,filename="),
                path,
                &format!(",cache.direct={direct},aio={aio}{discard}"),
            ),
        );

        let format = match d.resolved_format() {
            DiskFormat::Qcow2 => "qcow2",
            DiskFormat::Raw | DiskFormat::Auto => "raw",
        };

        a.pair(
            "-blockdev",
            format!("driver={format},node-name=disk{i},file=disk{i}-file{discard}"),
        );
        let serial = d.serial.clone().unwrap_or_else(|| format!("loophole{i}"));

        let blocks = match host.block_sizes.get(i).copied().flatten() {
            Some(n) if n > 512 => format!(",logical_block_size={n},physical_block_size={n}"),
            _ => String::new(),
        };

        let boot = bootindex(d.boot_order);
        let addr = disk_slot(i);
        a.pair(
            "-device",
            match d.bus {
                DiskBus::Nvme => format!(
                    "nvme,drive=disk{i},serial={},addr={addr:#x}{blocks}{boot}",
                    escaped(&serial)
                ),
                DiskBus::Virtio => {
                    let serial = d
                        .serial
                        .as_deref()
                        .map(|s| format!(",serial={}", escaped(s)))
                        .unwrap_or_default();

                    format!("virtio-blk-pci,drive=disk{i},addr={addr:#x}{serial}{blocks}{boot}")
                },
                DiskBus::Sata => {
                    let device = format!(
                        "ide-hd,drive=disk{i},bus=ide.{sata},serial={}{boot}",
                        escaped(&serial)
                    );
                    sata += 1;
                    device
                },
            },
        );
    }

    let drives = [
        (
            INSTALL_DRIVE,
            cfg.media.install.as_deref(),
            cfg.media.install_boot_order,
        ),
        (
            DRIVER_DRIVE,
            tools
                .driver_disc
                .as_deref()
                .filter(|_| cfg.media.driver_disc),
            None,
        ),
    ];

    for (i, (id, media, boot)) in drives.into_iter().enumerate() {
        let mut drive = OsString::from(format!("if=none,id=cd{i},media=cdrom,readonly=on"));

        if let Some(path) = media {
            drive.push(option(",file=", path, ""));
        }

        a.pair("-drive", drive);
        a.pair(
            "-device",
            format!("ide-cd,drive=cd{i},id={id},bus=ide.{i}{}", bootindex(boot)),
        );
    }

    if cfg.nics.is_empty() {
        a.pair("-nic", "none");
    }

    for (i, n) in cfg.nics.iter().enumerate() {
        let netdev = match n.backend {
            NetBackend::User => {
                let mut s = format!("user,id=net{i}");

                for f in &n.forwards {
                    let address = f.address.map(|a| a.to_string()).unwrap_or_default();
                    s += &format!(",hostfwd={}:{address}:{}-:{}", f.proto, f.host, f.guest);
                }

                OsString::from(s)
            },
            NetBackend::Tap => OsString::from(format!(
                "tap,id=net{i},ifname={},script=no,downscript=no",
                escaped(n.tap.as_deref().unwrap_or_default())
            )),
        };

        a.pair("-netdev", netdev);

        let model = match n.model {
            NicModel::E1000e => "e1000e",
            NicModel::Virtio => "virtio-net-pci",
        };

        let mac = n.mac.map(|m| format!(",mac={m}")).unwrap_or_default();
        a.pair(
            "-device",
            format!("{model},netdev=net{i},addr={:#x}{mac}", nic_slot(i)),
        );
    }

    a.pair(
        "-device",
        format!("qemu-xhci,id=xhci,addr={:#x}", slot::XHCI),
    );

    if cfg.input.tablet {
        a.pair("-device", "usb-tablet,bus=xhci.0");
    }

    for u in &cfg.usb {
        a.pair(
            "-device",
            format!(
                "usb-host,bus=xhci.0,vendorid={:#06x},productid={:#06x}",
                u.id.vendor, u.id.product
            ),
        );
    }

    for (i, p) in cfg.pci.iter().enumerate() {
        a.pair(
            "-device",
            format!(
                "pcie-root-port,id=rp{i},bus=pcie.0,chassis={},addr={:#x}",
                i + 1,
                slot::ROOT_PORTS + i as u32
            ),
        );

        let rombar = if p.rom_bar { "" } else { ",rombar=0" };
        a.pair(
            "-device",
            format!(
                "vfio-pci,host={},bus=rp{i}{rombar}{}",
                p.address,
                bootindex(p.boot_order)
            ),
        );
    }

    a.pair(
        "-qmp",
        option("unix:", &run.control(), ",server=on,wait=off"),
    );
    a.pair("-qmp", option("unix:", &run.qmp(), ",server=on,wait=off"));
    a.pair("-serial", option("file:", &run.serial(), ""));
    a.pair("-monitor", "none");
    a.pair("-pidfile", run.pid());

    if cfg.boot.menu {
        a.pair(
            "-boot",
            format!("menu=on,splash-time={}", cfg.boot.menu_timeout_ms),
        );
    }

    a.pair(
        "-display",
        option("dbus,p2p=on,gl=on,rendernode=", host.render_node, ""),
    );
    a.pair("-vga", "none");
    a.pair(
        "-device",
        format!(
            "virtio-vga-gl,id=gpu,blob=on,hostmem={}G,drm_native_context=on,addr={:#x}",
            cfg.gpu.window_gib,
            slot::GPU
        ),
    );

    if cfg.sound.enabled {
        let name = escaped(&cfg.name);

        let backend = match (host.pipewire, cfg.sound.microphone) {
            (false, _) => "none".to_owned(),
            (true, false) => format!("pipewire,out.stream-name={name}"),
            (true, true) => format!("pipewire,out.stream-name={name},in.stream-name={name}"),
        };

        a.pair("-audiodev", format!("{backend},id=snd0"));
        a.pair("-device", format!("ich9-intel-hda,addr={:#x}", slot::HDA));
        a.pair(
            "-device",
            if cfg.sound.microphone {
                "hda-micro,audiodev=snd0"
            } else {
                "hda-output,audiodev=snd0"
            },
        );
    }

    a.0.extend(cfg.machine.extra_args.iter().map(OsString::from));
    a.0
}
