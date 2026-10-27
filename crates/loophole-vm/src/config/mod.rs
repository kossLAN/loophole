mod ids;
mod legacy;
mod validate;

use std::fmt;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

pub use self::ids::{Mac, PciAddress, Resolution, UsbId};
pub use self::validate::{Issue, Section};
use crate::dirs;

macro_rules! choice {
    (pub enum $name:ident { $($variant:ident = $text:literal),+ $(,)? } default $default:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(
                #[serde(rename = $text)]
                $variant,
            )+
        }

        impl $name {
            pub const ALL: &[Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            pub fn index(self) -> usize {
                Self::ALL.iter().position(|&v| v == self).unwrap_or(0)
            }

            pub fn from_index(i: usize) -> Self {
                Self::ALL.get(i).copied().unwrap_or_default()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::$default
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::ALL.iter().copied().find(|v| v.as_str() == s).ok_or_else(|| {
                    let all: Vec<&str> = Self::ALL.iter().map(|v| v.as_str()).collect();
                    format!("expected one of: {} (not {s})", all.join(", "))
                })
            }
        }
    };
}

choice! { pub enum HugePages { Off = "off", Size2M = "2M", Size1G = "1G" } default Off }
choice! { pub enum DiskBus { Nvme = "nvme", Virtio = "virtio", Sata = "sata" } default Nvme }
choice! { pub enum DiskFormat { Auto = "auto", Qcow2 = "qcow2", Raw = "raw" } default Auto }
choice! { pub enum DiskCache { Auto = "auto", Direct = "direct", Writeback = "writeback" } default Auto }
choice! { pub enum NicModel { E1000e = "e1000e", Virtio = "virtio" } default E1000e }
choice! { pub enum NetBackend { User = "user", Tap = "tap" } default User }
choice! { pub enum Proto { Tcp = "tcp", Udp = "udp" } default Tcp }
choice! { pub enum Capture { Auto = "auto", Manual = "manual", Never = "never" } default Auto }
choice! { pub enum Windows { Eleven = "Windows 11", Ten = "Windows 10" } default Eleven }

const fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    #[serde(skip)]
    pub name: String,
    #[serde(default)]
    pub cpu: Cpu,
    #[serde(default)]
    pub memory: Memory,
    #[serde(default)]
    pub gpu: Gpu,
    #[serde(default)]
    pub media: Media,
    #[serde(default, rename = "disk", skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<Disk>,
    #[serde(default, rename = "nic", skip_serializing_if = "Vec::is_empty")]
    pub nics: Vec<Nic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci: Vec<PciPassthrough>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usb: Vec<UsbPassthrough>,
    #[serde(default)]
    pub sound: Sound,
    #[serde(default)]
    pub input: Input,
    #[serde(default)]
    pub boot: Boot,
    #[serde(default)]
    pub display: Display,
    #[serde(default)]
    pub machine: Machine,
    #[serde(default)]
    pub guest: Guest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cpu {
    pub sockets: u32,
    pub cores: u32,
    pub threads: u32,
    pub hyperv: bool,
}

impl Default for Cpu {
    fn default() -> Self {
        Self {
            sockets: 1,
            cores: 8,
            threads: 1,
            hyperv: true,
        }
    }
}

impl Cpu {
    pub const fn vcpus(&self) -> u32 {
        self.sockets
            .saturating_mul(self.cores)
            .saturating_mul(self.threads)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Memory {
    pub size_gib: u32,
    pub hugepages: HugePages,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            size_gib: 16,
            hugepages: HugePages::Off,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gpu {
    pub window_gib: u32,
}

impl Default for Gpu {
    fn default() -> Self {
        Self { window_gib: 8 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Media {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_boot_order: Option<u32>,
    pub driver_disc: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiskSource {
    Image(PathBuf),
    Device(PathBuf),
}

impl DiskSource {
    pub fn path(&self) -> &Path {
        match self {
            Self::Image(p) | Self::Device(p) => p,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "DiskToml", into = "DiskToml")]
pub struct Disk {
    pub source: DiskSource,
    pub bus: DiskBus,
    pub format: DiskFormat,
    pub cache: DiskCache,
    pub discard: bool,
    pub serial: Option<String>,
    pub boot_order: Option<u32>,
}

impl Disk {
    pub fn image(path: impl Into<PathBuf>) -> Self {
        Self {
            source: DiskSource::Image(path.into()),
            bus: DiskBus::Nvme,
            format: DiskFormat::Auto,
            cache: DiskCache::Auto,
            discard: true,
            serial: None,
            boot_order: None,
        }
    }

    pub fn device(path: impl Into<PathBuf>) -> Self {
        Self {
            source: DiskSource::Device(path.into()),
            ..Self::image("")
        }
    }

    pub fn resolved_format(&self) -> DiskFormat {
        match (self.format, &self.source) {
            (DiskFormat::Auto, DiskSource::Image(p))
                if p.extension().is_some_and(|e| e == "qcow2") =>
            {
                DiskFormat::Qcow2
            },
            (DiskFormat::Auto, _) => DiskFormat::Raw,
            (f, _) => f,
        }
    }

    pub const fn direct(&self) -> bool {
        match self.cache {
            DiskCache::Direct => true,
            DiskCache::Writeback => false,
            DiskCache::Auto => matches!(self.source, DiskSource::Device(_)),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskToml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<PathBuf>,
    #[serde(default)]
    bus: DiskBus,
    #[serde(default)]
    format: DiskFormat,
    #[serde(default)]
    cache: DiskCache,
    #[serde(default = "yes")]
    discard: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boot_order: Option<u32>,
}

impl TryFrom<DiskToml> for Disk {
    type Error = String;

    fn try_from(t: DiskToml) -> Result<Self, Self::Error> {
        let source = match (t.image, t.device) {
            (Some(p), None) => DiskSource::Image(p),
            (None, Some(p)) => DiskSource::Device(p),
            _ => return Err("a disk names either an image or a device".into()),
        };

        Ok(Self {
            source,
            bus: t.bus,
            format: t.format,
            cache: t.cache,
            discard: t.discard,
            serial: t.serial,
            boot_order: t.boot_order,
        })
    }
}

impl From<Disk> for DiskToml {
    fn from(d: Disk) -> Self {
        let (image, device) = match d.source {
            DiskSource::Image(p) => (Some(p), None),
            DiskSource::Device(p) => (None, Some(p)),
        };

        Self {
            image,
            device,
            bus: d.bus,
            format: d.format,
            cache: d.cache,
            discard: d.discard,
            serial: d.serial,
            boot_order: d.boot_order,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Nic {
    pub model: NicModel,
    pub backend: NetBackend,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tap: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<Mac>,
    #[serde(rename = "forward", skip_serializing_if = "Vec::is_empty")]
    pub forwards: Vec<Forward>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forward {
    #[serde(default)]
    pub proto: Proto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Ipv4Addr>,
    pub host: u16,
    pub guest: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PciPassthrough {
    pub address: PciAddress,
    #[serde(default = "yes")]
    pub rom_bar: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_order: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbPassthrough {
    pub id: UsbId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sound {
    pub enabled: bool,
    pub microphone: bool,
}

impl Default for Sound {
    fn default() -> Self {
        Self {
            enabled: true,
            microphone: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    pub tablet: bool,
}

impl Default for Input {
    fn default() -> Self {
        Self { tablet: true }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Boot {
    pub menu: bool,
    pub menu_timeout_ms: u32,
}

impl Default for Boot {
    fn default() -> Self {
        Self {
            menu: false,
            menu_timeout_ms: 3000,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Display {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_hz: Option<u32>,
    pub capture: Capture,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Guest {
    pub windows: Windows,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Machine {
    pub tpm: bool,
    pub options: String,
    pub extra_args: Vec<String>,
}

impl Default for Machine {
    fn default() -> Self {
        Self {
            tpm: true,
            options: String::new(),
            extra_args: Vec::new(),
        }
    }
}

impl VmConfig {
    pub fn new(name: &str) -> Self {
        let mut disk = Disk::image(dirs::data_dir(name).join("disk.qcow2"));
        disk.serial = Some("loophole0".into());
        disk.boot_order = Some(1);

        Self {
            name: name.into(),
            cpu: Cpu::default(),
            memory: Memory::default(),
            gpu: Gpu::default(),
            media: Media {
                install: None,
                install_boot_order: Some(2),
                driver_disc: true,
            },
            disks: vec![disk],
            nics: vec![Nic {
                mac: Some(Mac::random()),
                ..Nic::default()
            }],
            pci: Vec::new(),
            usb: Vec::new(),
            sound: Sound::default(),
            input: Input::default(),
            boot: Boot::default(),
            display: Display::default(),
            machine: Machine::default(),
            guest: Guest::default(),
        }
    }

    pub fn parse(name: &str, text: &str) -> Result<Self> {
        let table: toml::Table = toml::from_str(text)?;

        let mut cfg = if legacy::is_legacy(&table) {
            legacy::parse(table)?
        } else {
            toml::from_str::<Self>(text)?
        };

        cfg.name = name.into();
        Ok(cfg)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string(self)?)
    }

    pub fn owned_images(&self) -> Vec<&Path> {
        let data = dirs::data_dir(&self.name);

        self.disks
            .iter()
            .filter_map(|d| match &d.source {
                DiskSource::Image(p) if p.starts_with(&data) => Some(p.as_path()),
                DiskSource::Image(_) | DiskSource::Device(_) => None,
            })
            .collect()
    }

    pub fn free_image_path(&self, taken: &[PathBuf]) -> PathBuf {
        let data = dirs::data_dir(&self.name);
        let used = |p: &Path| {
            p.exists()
                || taken.iter().any(|t| t == p)
                || self.disks.iter().any(|d| d.source.path() == p)
        };

        (0..64)
            .map(|i| {
                data.join(if i == 0 {
                    "disk.qcow2".into()
                } else {
                    format!("disk{i}.qcow2")
                })
            })
            .find(|p| !used(p))
            .unwrap_or_else(|| data.join("disk.qcow2"))
    }
}

pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.');

    if !ok {
        bail!("a VM name uses letters, digits, '-', '_' and '.', and does not start with '.'");
    }

    Ok(())
}

pub fn load(name: &str) -> Result<VmConfig> {
    let path = dirs::config_dir().join(format!("{name}.toml"));
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    VmConfig::parse(name, &text).with_context(|| format!("in {}", path.display()))
}

pub fn load_all() -> Vec<Result<VmConfig>> {
    let Ok(entries) = fs::read_dir(dirs::config_dir()) else {
        return Vec::new();
    };

    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();

            if path.extension()? != "toml" {
                return None;
            }

            path.file_stem()?.to_str().map(String::from)
        })
        .collect();
    names.sort();
    names.iter().map(|name| load(name)).collect()
}

pub fn save(cfg: &VmConfig) -> Result<()> {
    check_name(&cfg.name)?;
    let dir = dirs::config_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.toml", cfg.name));
    let tmp = dir.join(format!(".{}.toml.tmp", cfg.name));
    fs::write(&tmp, cfg.to_toml()?)?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

pub fn remove(cfg: &VmConfig) -> Result<()> {
    match fs::remove_file(dirs::config_dir().join(format!("{}.toml", cfg.name))) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {},
    }

    let data = dirs::data_dir(&cfg.name);

    if data.exists() {
        fs::remove_dir_all(&data).with_context(|| format!("removing {}", data.display()))?;
    }

    Ok(())
}
