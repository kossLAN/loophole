use std::path::PathBuf;

use anyhow::{Context as _, Result};

use crate::driver_disc;

#[derive(Clone, Debug)]
pub struct Tools {
    pub qemu: PathBuf,
    pub qemu_img: PathBuf,
    pub ovmf_code: PathBuf,
    pub ovmf_vars: PathBuf,
    pub swtpm: PathBuf,
    pub driver_disc: Option<PathBuf>,
}

fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn required(name: &str) -> Result<PathBuf> {
    var(name).with_context(|| {
        format!("{name} is not set: run loophole from its Nix package or its development shell")
    })
}

impl Tools {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            qemu: required("LOOPHOLE_QEMU")?,
            qemu_img: required("LOOPHOLE_QEMU_IMG")?,
            ovmf_code: required("LOOPHOLE_OVMF_CODE")?,
            ovmf_vars: required("LOOPHOLE_OVMF_VARS")?,
            swtpm: required("LOOPHOLE_SWTPM")?,
            driver_disc: if driver_disc::pinned() {
                var("LOOPHOLE_DRIVER_DISC").filter(|p| p.exists())
            } else {
                driver_disc::cached().map(|d| d.path)
            },
        })
    }
}
