use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

use super::{
    Boot, Cpu, Disk, DiskBus, DiskCache, DiskFormat, DiskSource, Display, Gpu, Guest, HugePages,
    Input, Machine, Media, Memory, Nic, Sound, VmConfig,
};

const KEYS: [&str; 8] = [
    "cpus",
    "memory_gib",
    "gpu_window_gib",
    "disk",
    "disk_bus",
    "install_media",
    "driver_disc",
    "extra_args",
];

pub fn is_legacy(table: &toml::Table) -> bool {
    table
        .iter()
        .any(|(k, v)| KEYS.contains(&k.as_str()) && !(k == "disk" && v.is_array()))
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum LegacyBus {
    Nvme,
    Virtio,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Legacy {
    cpus: u32,
    memory_gib: u32,
    gpu_window_gib: u32,
    disk: PathBuf,
    #[serde(default)]
    disk_bus: Option<LegacyBus>,
    #[serde(default)]
    install_media: Option<PathBuf>,
    #[serde(default)]
    driver_disc: bool,
    #[serde(default)]
    extra_args: Vec<String>,
}

pub fn parse(table: toml::Table) -> Result<VmConfig> {
    let old: Legacy = toml::Value::Table(table).try_into()?;
    let virtio = matches!(old.disk_bus, Some(LegacyBus::Virtio));
    let disk = Disk {
        source: DiskSource::Image(old.disk),
        bus: if virtio {
            DiskBus::Virtio
        } else {
            DiskBus::Nvme
        },
        format: DiskFormat::Auto,
        cache: DiskCache::Auto,
        discard: true,
        serial: (!virtio).then(|| "wormhole0".into()),
        boot_order: Some(1),
    };

    Ok(VmConfig {
        name: String::new(),
        cpu: Cpu {
            sockets: 1,
            cores: old.cpus,
            threads: 1,
            hyperv: true,
        },
        memory: Memory {
            size_gib: old.memory_gib,
            hugepages: HugePages::Off,
        },
        gpu: Gpu {
            window_gib: old.gpu_window_gib,
        },
        media: Media {
            install: old.install_media,
            install_boot_order: Some(2),
            driver_disc: old.driver_disc,
        },
        disks: vec![disk],
        nics: vec![Nic::default()],
        pci: Vec::new(),
        usb: Vec::new(),
        sound: Sound::default(),
        input: Input::default(),
        boot: Boot::default(),
        display: Display::default(),
        machine: Machine {
            tpm: true,
            options: String::new(),
            extra_args: old.extra_args,
        },
        guest: Guest::default(),
    })
}
