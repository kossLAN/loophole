use std::path::Path;

use loophole_vm::config::{
    Capture, DiskBus, DiskCache, DiskFormat, HugePages, NetBackend, NicModel, Proto, Section,
    Windows,
};
use loophole_vm::host;
use slint::{ModelRc, SharedString, VecModel};

use crate::ui::{DiskEdit, DiskKind, EditOptions};

pub(super) fn text(s: impl Into<SharedString>) -> SharedString {
    s.into()
}

pub(super) fn int(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

pub(super) fn count(v: i32) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(0)
}

pub(super) fn index(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(0)
}

pub(super) fn gpu_window_sizes(vram: u64) -> Vec<u32> {
    let gib = vram.div_ceil(1 << 30).min(64);

    let Some(max_power) = gib.checked_ilog2() else {
        return Vec::new();
    };

    (0..=max_power).map(|power| 1_u32 << power).collect()
}

pub(super) fn gpu_window_index(sizes: &[u32], gib: u32) -> i32 {
    let selected = sizes
        .iter()
        .position(|&size| size >= gib)
        .unwrap_or_else(|| sizes.len().saturating_sub(1));

    index(selected)
}

pub(super) fn optional(s: &str) -> Option<String> {
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

pub(super) fn hugepage_pools() -> [Option<(&'static str, u64)>; 2] {
    [(HugePages::Size2M, "2 MiB"), (HugePages::Size1G, "1 GiB")].map(|(size, label)| {
        host::hugepages(size)
            .filter(|&(total, _)| total > 0)
            .map(|(_, free)| (label, free))
    })
}

pub(super) fn hugepage_hints(pools: &[Option<(&str, u64)>; 2]) -> Vec<SharedString> {
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

pub(super) fn file_name(p: &Path) -> String {
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

pub(super) const TABS: usize = 7;

pub(super) const fn create_tab(section: Section) -> usize {
    match section {
        Section::General => 0,
        s => tab(s) + 1,
    }
}

pub(super) fn labels<T>(all: &[T], label: fn(T) -> &'static str) -> ModelRc<SharedString>
where
    T: Copy,
{
    ModelRc::new(VecModel::from(
        all.iter().map(|&v| text(label(v))).collect::<Vec<_>>(),
    ))
}

pub(super) fn options(gpu_window_sizes: &[u32]) -> EditOptions {
    EditOptions {
        hugepages: labels(HugePages::ALL, |h| match h {
            HugePages::Off => "Off",
            HugePages::Size2M => "2 MiB pages",
            HugePages::Size1G => "1 GiB pages",
        }),
        gpu_window_sizes: ModelRc::new(VecModel::from(
            gpu_window_sizes
                .iter()
                .map(|size| text(format!("{size} GiB")))
                .collect::<Vec<_>>(),
        )),
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

pub(super) fn disk_info(d: &DiskEdit) -> (String, String) {
    let path = Path::new(d.path.as_str());

    match d.kind {
        DiskKind::NewImage => (
            "New disk".into(),
            format!("{} GiB qcow2, created on apply", d.new_gib),
        ),
        DiskKind::Device => {
            let dev = host::block_device(path);
            let title = dev
                .as_ref()
                .map(|b| b.model.clone())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| file_name(path));
            let info = dev.map_or_else(
                || "host disk".into(),
                |b| format!("host disk  {}  {}", gib(b.size), b.node.display()),
            );

            (title, info)
        },
        DiskKind::Image => {
            let size = std::fs::metadata(path)
                .map_or(String::new(), |m| format!("  {} on the host", gib(m.len())));

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
