use std::fs;
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use rustix::process::{Resource, Rlimit};

use crate::argv::{Host, RunPaths, argv};
use crate::config::{DiskBus, DiskSource, HugePages, Issue, NetBackend, Section, VmConfig};
use crate::dirs;
use crate::host::{self, Passthrough};
use crate::tools::Tools;

pub fn running_pid(name: &str) -> Option<i32> {
    let run = RunPaths::new(name);
    let pid: i32 = fs::read_to_string(run.pid()).ok()?.trim().parse().ok()?;
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let dir = run.dir.to_string_lossy();

    String::from_utf8_lossy(&cmdline)
        .contains(dir.as_ref())
        .then_some(pid)
}

pub fn log_tail(name: &str, lines: usize) -> String {
    let text = fs::read_to_string(RunPaths::new(name).log()).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn create_disk(tools: &Tools, path: &Path, gib: u32) -> Result<()> {
    if path.exists() {
        bail!("{} exists already", path.display());
    }

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }

    let out = Command::new(&tools.qemu_img)
        .args(["create", "-q", "-f", "qcow2"])
        .arg(path)
        .arg(format!("{gib}G"))
        .output()
        .context("running qemu-img")?;

    if !out.status.success() {
        bail!(
            "qemu-img create: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    Ok(())
}

pub fn memlock_needed(cfg: &VmConfig) -> u64 {
    (u64::from(cfg.memory.size_gib) + u64::from(cfg.gpu.window_gib) + 1) << 30
}

pub fn preflight(cfg: &VmConfig) -> Vec<Issue> {
    let mut out = Vec::new();
    let mut err = |s, m: String| out.push(Issue::error(s, m));

    for (i, d) in cfg.disks.iter().enumerate() {
        let section = Section::Disk(i);

        match &d.source {
            DiskSource::Image(p) => {
                if !p.as_os_str().is_empty() && !p.is_file() {
                    err(section, format!("{} does not exist", p.display()));
                }
            },
            DiskSource::Device(p) => {
                let is_block = fs::metadata(p).is_ok_and(|m| m.file_type().is_block_device());

                if !is_block {
                    err(
                        section,
                        format!("{} is not a block device here", p.display()),
                    );
                    continue;
                }

                let Some(dev) = host::block_device(p) else {
                    continue;
                };

                if let Some(why) = &dev.in_use {
                    err(section, format!("{} is {why} on the host", p.display()));
                }

                if !dev.accessible {
                    err(
                        section,
                        format!(
                            "no access to {}: add it to programs.loophole.blockDevices",
                            dev.node.display()
                        ),
                    );
                }

                if d.bus == DiskBus::Sata && dev.logical_block != 512 {
                    err(
                        section,
                        format!(
                            "{}-byte sectors need NVMe or virtio-blk, not SATA",
                            dev.logical_block
                        ),
                    );
                }
            },
        }
    }

    if let Some(iso) = &cfg.media.install
        && !iso.is_file()
    {
        err(Section::Media, format!("{} does not exist", iso.display()));
    }

    for (i, p) in cfg.pci.iter().enumerate() {
        let status = host::passthrough(p.address);

        if status != Passthrough::Ready {
            err(
                Section::Pci(i),
                format!("{}: {}", p.address, status.describe()),
            );
        }
    }

    if !cfg.pci.is_empty() {
        let need = memlock_needed(cfg);

        if let Some(limit) = host::memlock()
            && limit < need
        {
            err(
                Section::Pci(0),
                format!(
                    "PCI passthrough locks {} GiB, the limit is {} MiB (NixOS: programs.loophole.vfio.enable, then log in again)",
                    need >> 30,
                    limit >> 20
                ),
            );
        }
    }

    if cfg.memory.hugepages != HugePages::Off {
        let page = match cfg.memory.hugepages {
            HugePages::Size1G => 1 << 30,
            HugePages::Size2M | HugePages::Off => 2 << 20,
        };

        let need = u64::from(cfg.memory.size_gib) << 30;

        match host::hugepages(cfg.memory.hugepages) {
            Some((_, free)) if free * page >= need => {},
            Some((_, free)) => err(
                Section::Memory,
                format!(
                    "{} hugepages free ({} GiB), {} GiB needed (NixOS: programs.loophole.hugepages)",
                    free,
                    (free * page) >> 30,
                    need >> 30
                ),
            ),
            None => err(
                Section::Memory,
                format!("this kernel has no {} hugepages", cfg.memory.hugepages),
            ),
        }
    }

    let taps = host::taps();

    for (i, n) in cfg.nics.iter().enumerate() {
        let Some(name) = n.tap.as_ref().filter(|_| n.backend == NetBackend::Tap) else {
            continue;
        };

        match taps.iter().find(|t| t.name == *name) {
            None => err(Section::Nic(i), format!("there is no TAP device {name}")),
            Some(t) if !t.accessible => err(
                Section::Nic(i),
                format!("{name} belongs to another user (NixOS: virtualOwner)"),
            ),
            Some(_) => {},
        }
    }

    let usb = host::usb_devices();

    for (i, u) in cfg.usb.iter().enumerate() {
        match usb.iter().find(|d| d.id == u.id) {
            None => out.push(Issue::warning(
                Section::Usb(i),
                format!("{} is not plugged in, it attaches when it is", u.id),
            )),
            Some(d) if !d.accessible => out.push(Issue::error(
                Section::Usb(i),
                format!(
                    "no access to {} ({}): add it to programs.loophole.usbDevices",
                    d.name, u.id
                ),
            )),
            Some(_) => {},
        }
    }

    out
}

fn detached(cmd: &mut Command, memlock: bool) -> &mut Command {
    unsafe {
        cmd.pre_exec(move || {
            rustix::process::setsid().map_err(std::io::Error::from)?;

            if memlock {
                let max = rustix::process::getrlimit(Resource::Memlock).maximum;
                let _ = rustix::process::setrlimit(
                    Resource::Memlock,
                    Rlimit {
                        current: max,
                        maximum: max,
                    },
                );
            }

            Ok(())
        })
    }
}

fn wait_for_socket(
    path: &Path,
    what: &str,
    timeout: Duration,
    alive: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let deadline = Instant::now() + timeout;

    loop {
        alive()?;

        if UnixStream::connect(path).is_ok() {
            return Ok(());
        }

        if Instant::now() > deadline {
            bail!(
                "{what} did not open {} in {} s",
                path.display(),
                timeout.as_secs()
            );
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start_tpm(cfg: &VmConfig, tools: &Tools, run: &RunPaths) -> Result<()> {
    let state = dirs::data_dir(&cfg.name).join("tpm");
    fs::create_dir_all(&state)?;
    let status = Command::new(&tools.swtpm)
        .arg("socket")
        .arg("--tpm2")
        .arg("--tpmstate")
        .arg(format!("dir={}", state.display()))
        .arg("--ctrl")
        .arg(format!("type=unixio,path={}", run.tpm().display()))
        .arg("--log")
        .arg(format!("file={}", run.tpm_log().display()))
        .arg("--pid")
        .arg(format!("file={}", run.tpm_pid().display()))
        .args(["--terminate", "--daemon"])
        .stdin(Stdio::null())
        .status()
        .context("starting swtpm")?;

    if !status.success() {
        bail!(
            "swtpm failed ({status}): {}",
            fs::read_to_string(run.tpm_log()).unwrap_or_default().trim()
        );
    }

    let deadline = Instant::now() + Duration::from_secs(5);

    while !run.tpm().exists() {
        if Instant::now() > deadline {
            bail!("swtpm did not create {}", run.tpm().display());
        }

        std::thread::sleep(Duration::from_millis(20));
    }

    Ok(())
}

fn stop_tpm(run: &RunPaths) {
    let pid = fs::read_to_string(run.tpm_pid())
        .ok()
        .and_then(|p| p.trim().parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw);

    if let Some(pid) = pid {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
}

fn ensure_vars(cfg: &VmConfig, tools: &Tools) -> Result<PathBuf> {
    let data = dirs::data_dir(&cfg.name);
    fs::create_dir_all(&data)?;

    let vars = data.join("OVMF_VARS.fd");

    if !vars.exists() {
        fs::copy(&tools.ovmf_vars, &vars).context("copying the UEFI variables")?;
        fs::set_permissions(&vars, fs::Permissions::from_mode(0o600))?;
    }

    Ok(vars)
}

pub fn start(cfg: &VmConfig, tools: &Tools) -> Result<Child> {
    if running_pid(&cfg.name).is_some() {
        bail!("{} is running already", cfg.name);
    }

    let problems: Vec<String> = cfg
        .validate()
        .into_iter()
        .chain(preflight(cfg))
        .filter(|i| i.fatal)
        .map(|i| i.message)
        .collect();

    if !problems.is_empty() {
        bail!("{}", problems.join("  "));
    }

    let nodes = host::render_nodes();
    let gpu = host::gpu(&nodes).context("no amdgpu render node (/dev/dri/renderD*)")?;
    let vars = ensure_vars(cfg, tools)?;
    let run = RunPaths::new(&cfg.name);

    fs::create_dir_all(&run.dir)?;

    for stale in [
        run.control(),
        run.qmp(),
        run.clipboard(),
        run.tpm(),
        run.tpm_pid(),
        run.pid(),
    ] {
        let _ = fs::remove_file(stale);
    }

    if cfg.machine.tpm {
        start_tpm(cfg, tools, &run)?;
    }

    let block_sizes: Vec<Option<u32>> = cfg
        .disks
        .iter()
        .map(|d| match &d.source {
            DiskSource::Device(p) => host::block_device(p).map(|b| b.logical_block),
            DiskSource::Image(_) => None,
        })
        .collect();
    let host = Host {
        render_node: &gpu.path,
        pipewire: host::pipewire(),
        io_uring: host::io_uring(),
        block_sizes: &block_sizes,
    };

    let argv = argv(cfg, tools, &run, &vars, &host);
    let log = fs::File::create(run.log())?;
    let spawned = detached(
        Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&run.dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log),
        !cfg.pci.is_empty(),
    )
    .spawn()
    .with_context(|| format!("starting {}", argv[0].to_string_lossy()));

    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            stop_tpm(&run);
            return Err(e);
        },
    };

    let mut alive = || match child.try_wait()? {
        Some(status) => bail!("QEMU exited ({status}):\n{}", log_tail(&cfg.name, 20)),
        None => Ok(()),
    };

    if let Err(e) = wait_for_socket(&run.control(), "QEMU", Duration::from_secs(30), &mut alive) {
        let _ = child.kill();
        let _ = child.wait();
        stop_tpm(&run);
        return Err(e);
    }

    Ok(child)
}
