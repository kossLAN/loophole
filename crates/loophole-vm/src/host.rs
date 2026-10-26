use std::fs;
use std::path::{Path, PathBuf};

use rustix::fs::Access;
use rustix::process::Resource;

use crate::config::{HugePages, PciAddress, UsbId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderNode {
    pub path: PathBuf,
    pub driver: String,
    pub vram: Option<u64>,
    // PCI address with vendor and device id, e.g. "0000:03:00.0 [1002:7550]".
    pub description: String,
}

struct Sys {
    root: PathBuf,
}

impl Default for Sys {
    fn default() -> Self {
        Self { root: "/".into() }
    }
}

impl Sys {
    fn path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(
            rel.as_ref()
                .strip_prefix("/")
                .unwrap_or_else(|_| rel.as_ref()),
        )
    }

    fn read(&self, rel: impl AsRef<Path>) -> Option<String> {
        Some(fs::read_to_string(self.path(rel)).ok()?.trim().to_owned())
    }

    fn number(&self, rel: impl AsRef<Path>) -> Option<u64> {
        let text = self.read(rel)?;
        text.strip_prefix("0x").map_or_else(
            || text.parse().ok(),
            |hex| u64::from_str_radix(hex, 16).ok(),
        )
    }

    fn link_name(&self, rel: impl AsRef<Path>) -> Option<String> {
        let target = fs::read_link(self.path(rel)).ok()?;
        Some(target.file_name()?.to_string_lossy().into_owned())
    }

    fn list(&self, rel: impl AsRef<Path>) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.path(rel))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn exists(&self, rel: impl AsRef<Path>) -> bool {
        self.path(rel).exists()
    }

    fn accessible(&self, rel: impl AsRef<Path>) -> bool {
        rustix::fs::access(self.path(rel), Access::READ_OK | Access::WRITE_OK).is_ok()
    }

    fn udev(&self, id: &str, key: &str) -> Option<String> {
        let text = self.read(format!("/run/udev/data/{id}"))?;
        let prefix = format!("E:{key}=");
        text.lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .map(String::from)
    }
}

pub fn render_nodes() -> Vec<RenderNode> {
    render_nodes_in(&Sys::default())
}

fn render_nodes_in(sys: &Sys) -> Vec<RenderNode> {
    sys.list("/sys/class/drm")
        .into_iter()
        .filter(|n| n.starts_with("renderD"))
        .map(|n| {
            let dev = format!("/sys/class/drm/{n}/device");
            let ids = match (
                sys.read(format!("{dev}/vendor")),
                sys.read(format!("{dev}/device")),
            ) {
                (Some(v), Some(d)) => format!(
                    " [{}:{}]",
                    v.trim_start_matches("0x"),
                    d.trim_start_matches("0x")
                ),
                _ => String::new(),
            };
            RenderNode {
                path: Path::new("/dev/dri").join(&n),
                driver: sys
                    .link_name(format!("{dev}/driver"))
                    .unwrap_or_else(|| "?".into()),
                vram: sys.number(format!("{dev}/mem_info_vram_total")),
                description: format!("{}{ids}", sys.link_name(&dev).unwrap_or_default()),
            }
        })
        .collect()
}

// virglrenderer opens the first amdgpu render node whatever -display names. The display must
// name the same one to import the guest's frames.
pub fn gpu(nodes: &[RenderNode]) -> Option<&RenderNode> {
    nodes.iter().find(|n| n.driver == "amdgpu")
}

fn can_open(path: &Path) -> bool {
    rustix::fs::access(path, Access::READ_OK | Access::WRITE_OK).is_ok()
}

pub fn pipewire() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| Path::new(&d).join("pipewire-0").exists())
}

pub fn io_uring() -> bool {
    Sys::default()
        .number("/proc/sys/kernel/io_uring_disabled")
        .is_none_or(|v| v == 0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockDevice {
    pub path: PathBuf,
    pub node: PathBuf,
    pub model: String,
    pub serial: String,
    pub size: u64,
    pub logical_block: u32,
    pub in_use: Option<String>,
    pub accessible: bool,
}

pub fn block_devices() -> Vec<BlockDevice> {
    block_devices_in(&Sys::default())
}

pub fn block_device(path: &Path) -> Option<BlockDevice> {
    let node = fs::canonicalize(path).ok()?;
    block_devices()
        .into_iter()
        .find(|d| d.node == node || d.path == path)
}

// Model and serial names first, then WWN and EUI ones.
fn by_id_rank(name: &str) -> u8 {
    u8::from(name.starts_with("wwn-") || name.contains("-eui.") || name.starts_with("nvme-nvme."))
}

fn block_devices_in(sys: &Sys) -> Vec<BlockDevice> {
    let by_id: Vec<(String, String)> = sys
        .list("/dev/disk/by-id")
        .into_iter()
        .filter(|n| !n.contains("-part"))
        .filter_map(|n| Some((sys.link_name(format!("/dev/disk/by-id/{n}"))?, n)))
        .collect();
    let mounts = sys.read("/proc/mounts").unwrap_or_default();
    let swaps = sys.read("/proc/swaps").unwrap_or_default();
    let skip = ["loop", "ram", "zram", "dm-", "md", "sr", "nbd"];
    sys.list("/sys/block")
        .into_iter()
        .filter(|n| !skip.iter().any(|s| n.starts_with(s)))
        .filter(|n| sys.exists(format!("/sys/block/{n}/device")))
        .map(|name| {
            let block = format!("/sys/block/{name}");
            let node = PathBuf::from(format!("/dev/{name}"));
            let path = by_id
                .iter()
                .filter(|(target, _)| *target == name)
                .min_by_key(|(_, id)| (by_id_rank(id), id.len()))
                .map_or_else(
                    || node.clone(),
                    |(_, id)| PathBuf::from(format!("/dev/disk/by-id/{id}")),
                );
            let parts: Vec<String> = sys
                .list(&block)
                .into_iter()
                .filter(|p| p.starts_with(&name) && sys.exists(format!("{block}/{p}/partition")))
                .collect();
            let nodes: Vec<String> = std::iter::once(name.clone())
                .chain(parts.iter().cloned())
                .collect();
            let mounted = mounts.lines().find_map(|l| {
                let mut fields = l.split_whitespace();
                let (dev, at) = (fields.next()?, fields.next()?);
                let dev = dev.strip_prefix("/dev/")?;
                nodes
                    .iter()
                    .any(|n| n == dev)
                    .then(|| format!("mounted at {at}"))
            });
            let swap = swaps.lines().skip(1).find_map(|l| {
                let dev = l.split_whitespace().next()?.strip_prefix("/dev/")?;
                nodes
                    .iter()
                    .any(|n| n == dev)
                    .then(|| "used as swap".to_owned())
            });
            let held = nodes.iter().find_map(|n| {
                let dir = if *n == name {
                    format!("{block}/holders")
                } else {
                    format!("{block}/{n}/holders")
                };
                let holders = sys.list(dir);
                (!holders.is_empty()).then(|| format!("in use by {}", holders.join(", ")))
            });
            BlockDevice {
                model: sys
                    .read(format!("{block}/device/model"))
                    .unwrap_or_default(),
                serial: sys
                    .read(format!("{block}/device/serial"))
                    .unwrap_or_default(),
                size: sys.number(format!("{block}/size")).unwrap_or(0) * 512,
                logical_block: sys
                    .number(format!("{block}/queue/logical_block_size"))
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(512),
                in_use: mounted.or(swap).or(held),
                accessible: sys.accessible(&node),
                path,
                node,
            }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciDevice {
    pub address: PciAddress,
    pub vendor: u16,
    pub device: u16,
    pub class: u32,
    pub name: String,
    pub driver: Option<String>,
    pub iommu_group: Option<u32>,
    pub contents: Vec<String>,
}

pub fn pci_devices() -> Vec<PciDevice> {
    pci_devices_in(&Sys::default())
}

fn usb_products(sys: &Sys, dir: &str, out: &mut Vec<String>) {
    for entry in sys.list(dir) {
        let path = format!("{dir}/{entry}");
        if !entry.contains('-') || entry.contains(':') {
            continue;
        }
        if let Some(product) = sys.read(format!("{path}/product")) {
            out.push(product);
        }
        usb_products(sys, &path, out);
    }
}

// Storage, network, display, multimedia, communication, input, serial bus and wireless controllers.
const PASSABLE: [u32; 8] = [0x01, 0x02, 0x03, 0x04, 0x07, 0x09, 0x0c, 0x0d];
const SMBUS: u32 = 0x0c05;

fn pci_devices_in(sys: &Sys) -> Vec<PciDevice> {
    sys.list("/sys/bus/pci/devices")
        .into_iter()
        .filter_map(|bdf| {
            let address: PciAddress = bdf.parse().ok()?;
            let dir = format!("/sys/bus/pci/devices/{bdf}");
            let class = u32::try_from(sys.number(format!("{dir}/class"))?).ok()?;
            if !PASSABLE.contains(&(class >> 16)) || class >> 8 == SMBUS {
                return None;
            }
            let id = |f: &str| {
                let v = sys.number(format!("{dir}/{f}"))?;
                u16::try_from(v).ok()
            };
            let (vendor, device) = (id("vendor")?, id("device")?);
            let udev = format!("+pci:{bdf}");
            let name = match (
                sys.udev(&udev, "ID_VENDOR_FROM_DATABASE"),
                sys.udev(&udev, "ID_MODEL_FROM_DATABASE"),
            ) {
                (Some(v), Some(m)) => format!("{v} {m}"),
                (None, Some(m)) => m,
                _ => format!("{vendor:04x}:{device:04x}"),
            };
            let mut contents = Vec::new();
            for ctrl in sys.list(format!("{dir}/nvme")) {
                contents.extend(
                    sys.list(format!("{dir}/nvme/{ctrl}"))
                        .into_iter()
                        .filter(|n| n.starts_with(&ctrl) && n.contains('n')),
                );
            }
            for bus in sys.list(&dir).into_iter().filter(|e| e.starts_with("usb")) {
                usb_products(sys, &format!("{dir}/{bus}"), &mut contents);
            }
            Some(PciDevice {
                address,
                vendor,
                device,
                class,
                name,
                driver: sys.link_name(format!("{dir}/driver")),
                iommu_group: sys
                    .link_name(format!("{dir}/iommu_group"))
                    .and_then(|g| g.parse().ok()),
                contents,
            })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Passthrough {
    Ready,
    Missing,
    NoIommu,
    RenderGpu,
    NotVfio(Option<String>),
    GroupShared(Vec<String>),
    NoAccess(PathBuf),
}

impl Passthrough {
    pub fn describe(&self) -> String {
        match self {
            Self::Ready => "ready".into(),
            Self::Missing => "not in this machine".into(),
            Self::NoIommu => "no IOMMU group: turn on the IOMMU (AMD-Vi / VT-d)".into(),
            Self::RenderGpu => "the GPU the VMs render on".into(),
            Self::NotVfio(driver) => format!(
                "bound to {}, not vfio-pci: add it to programs.loophole.vfio.devices",
                driver.as_deref().unwrap_or("no driver")
            ),
            Self::GroupShared(others) => format!(
                "its IOMMU group also holds {}, which must go to vfio-pci too",
                others.join(", ")
            ),
            Self::NoAccess(path) => format!("no access to {}", path.display()),
        }
    }
}

const VFIO_DRIVERS: [&str; 3] = ["vfio-pci", "pci-stub", "pcieport"];

pub fn passthrough(address: PciAddress) -> Passthrough {
    passthrough_in(&Sys::default(), address)
}

fn passthrough_in(sys: &Sys, address: PciAddress) -> Passthrough {
    let dir = format!("/sys/bus/pci/devices/{address}");
    if !sys.exists(&dir) {
        return Passthrough::Missing;
    }
    let bdf = address.to_string();
    if gpu(&render_nodes_in(sys)).is_some_and(|n| n.description.starts_with(&bdf)) {
        return Passthrough::RenderGpu;
    }
    let Some(group) = sys.link_name(format!("{dir}/iommu_group")) else {
        return Passthrough::NoIommu;
    };
    let driver = sys.link_name(format!("{dir}/driver"));
    if driver.as_deref() != Some("vfio-pci") {
        return Passthrough::NotVfio(driver);
    }
    let blockers: Vec<String> = sys
        .list(format!("/sys/kernel/iommu_groups/{group}/devices"))
        .into_iter()
        .filter(|other| *other != bdf)
        .filter_map(|other| {
            let d = sys.link_name(format!("/sys/bus/pci/devices/{other}/driver"))?;
            (!VFIO_DRIVERS.contains(&d.as_str())).then(|| format!("{other} ({d})"))
        })
        .collect();
    if !blockers.is_empty() {
        return Passthrough::GroupShared(blockers);
    }
    for dev in ["/dev/vfio/vfio".to_owned(), format!("/dev/vfio/{group}")] {
        if !sys.accessible(&dev) {
            return Passthrough::NoAccess(dev.into());
        }
    }
    Passthrough::Ready
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbDevice {
    pub id: UsbId,
    pub name: String,
    pub accessible: bool,
}

pub fn usb_devices() -> Vec<UsbDevice> {
    let sys = Sys::default();
    sys.list("/sys/bus/usb/devices")
        .into_iter()
        .filter_map(|e| {
            let dir = format!("/sys/bus/usb/devices/{e}");
            let id: UsbId = format!(
                "{}:{}",
                sys.read(format!("{dir}/idVendor"))?,
                sys.read(format!("{dir}/idProduct"))?
            )
            .parse()
            .ok()?;
            if id.vendor == 0x1d6b {
                return None;
            }
            let name = [
                sys.read(format!("{dir}/manufacturer")),
                sys.read(format!("{dir}/product")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            let node = format!(
                "/dev/bus/usb/{:03}/{:03}",
                sys.number(format!("{dir}/busnum")).unwrap_or(0),
                sys.number(format!("{dir}/devnum")).unwrap_or(0)
            );
            Some(UsbDevice {
                id,
                name: if name.is_empty() {
                    id.to_string()
                } else {
                    name
                },
                accessible: sys.accessible(node),
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct Tap {
    pub name: String,
    pub bridge: Option<String>,
    // QEMU, as this user, may attach to it.
    pub accessible: bool,
}

// IFF_TAP in tun_flags.
const IFF_TAP: u64 = 0x2;

pub fn taps() -> Vec<Tap> {
    let sys = Sys::default();
    let uid = u64::from(rustix::process::getuid().as_raw());
    let gids: Vec<u64> = rustix::process::getgroups()
        .unwrap_or_default()
        .into_iter()
        .chain([rustix::process::getgid()])
        .map(|g| u64::from(g.as_raw()))
        .collect();
    sys.list("/sys/class/net")
        .into_iter()
        .filter(|n| {
            sys.number(format!("/sys/class/net/{n}/tun_flags"))
                .is_some_and(|f| f & IFF_TAP != 0)
        })
        .map(|name| {
            // -1 (unparsed here) when unset: only root may attach then.
            let owner = sys.number(format!("/sys/class/net/{name}/owner"));
            let group = sys.number(format!("/sys/class/net/{name}/group"));
            Tap {
                bridge: sys.link_name(format!("/sys/class/net/{name}/master")),
                accessible: owner == Some(uid) || group.is_some_and(|g| gids.contains(&g)),
                name,
            }
        })
        .collect()
}

// Hard memlock limit in bytes. None is unlimited.
pub fn memlock() -> Option<u64> {
    rustix::process::getrlimit(Resource::Memlock).maximum
}

// Bytes free to unprivileged users on the filesystem holding `path` or its nearest existing parent.
pub fn free_space(path: &Path) -> Option<u64> {
    let dir = path.ancestors().find(|p| p.exists())?;
    let st = rustix::fs::statvfs(dir).ok()?;
    Some(st.f_bavail.saturating_mul(st.f_frsize))
}

// (total, free) pages of the pool.
pub fn hugepages(size: HugePages) -> Option<(u64, u64)> {
    let kb = match size {
        HugePages::Off => return None,
        HugePages::Size2M => 2048,
        HugePages::Size1G => 1 << 20,
    };
    let sys = Sys::default();
    let dir = format!("/sys/kernel/mm/hugepages/hugepages-{kb}kB");
    Some((
        sys.number(format!("{dir}/nr_hugepages"))?,
        sys.number(format!("{dir}/free_hugepages"))?,
    ))
}

pub fn memory_total() -> u64 {
    let meminfo = Sys::default().read("/proc/meminfo").unwrap_or_default();
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

#[derive(Clone, Debug)]
pub struct Check {
    pub what: &'static str,
    pub ok: bool,
    pub detail: String,
    pub required: bool,
}

pub fn checks() -> Vec<Check> {
    let sys = Sys::default();
    let nodes = render_nodes();
    let gpu = gpu(&nodes);
    let groups = sys.list("/sys/kernel/iommu_groups").len();
    let vfio = sys.exists("/sys/bus/pci/drivers/vfio-pci");
    let memlock = memlock();
    vec![
        Check {
            what: "KVM",
            ok: can_open(Path::new("/dev/kvm")),
            detail: "/dev/kvm (the kvm group)".into(),
            required: true,
        },
        Check {
            what: "GPU",
            ok: gpu.is_some_and(|n| can_open(&n.path)),
            detail: gpu.map_or_else(
                || "no amdgpu render node".into(),
                |n| format!("{} {} (the render group)", n.path.display(), n.description),
            ),
            required: true,
        },
        Check {
            what: "Sound",
            ok: pipewire(),
            detail: "PipeWire ($XDG_RUNTIME_DIR/pipewire-0), VMs are silent without it".into(),
            required: false,
        },
        Check {
            what: "IOMMU",
            ok: groups > 0,
            detail: if groups > 0 {
                format!("{groups} groups")
            } else {
                "off, PCI passthrough needs it (AMD-Vi / VT-d in the firmware)".into()
            },
            required: false,
        },
        Check {
            what: "VFIO",
            ok: vfio && sys.accessible("/dev/vfio/vfio"),
            detail: if vfio {
                "vfio-pci loaded".into()
            } else {
                "vfio-pci not loaded (NixOS: programs.loophole.vfio.enable)".into()
            },
            required: false,
        },
        Check {
            what: "Locked mem",
            ok: memlock.is_none_or(|m| m >= 1 << 34),
            detail: memlock.map_or_else(
                || "unlimited".into(),
                |m| format!("{} MiB, PCI passthrough locks all guest memory", m >> 20),
            ),
            required: false,
        },
    ]
}
