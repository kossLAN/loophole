mod disc;
mod editor;
mod gl;
mod manager;
mod models;
mod picker;
mod screen;
mod theme;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use loophole_vm::config::Resolution;
use loophole_vm::qmp::{self, Qmp};
use loophole_vm::{Capture, RunPaths, Tools, config, driver_disc, host, run};
use slint::ComponentHandle as _;
use slint::winit_030::winit::event::{DeviceEvent, DeviceId};
use slint::winit_030::winit::event_loop::ActiveEventLoop;
use slint::winit_030::{CustomApplicationHandler, EventResult};

use crate::manager::Manager;
use crate::screen::{Options, Screen};
pub use crate::ui::{MainWindow, VmItem};

#[derive(Parser)]
#[command(version, about = "Run Windows VMs on the host's AMD GPU and show them")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show a running VM's screen
    View(ViewArgs),
    /// List the VMs and whether they run
    List,
    /// Start a VM without a window
    Start { name: String },
    /// Ask a VM's guest to shut down (--force: end QEMU)
    Stop {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Check that this host can run VMs, and NAME if given
    Check { name: Option<String> },
}

#[derive(clap::Args)]
struct ViewArgs {
    /// The VM to show
    #[arg(required_unless_present = "qmp")]
    name: Option<String>,
    /// Instead, the QMP socket of any QEMU started with -display dbus,p2p=on
    #[arg(long, conflicts_with = "name")]
    qmp: Option<PathBuf>,
    /// Guest desktop size, e.g. 2560x1440, instead of the VM's setting or the window's size
    #[arg(long)]
    resolution: Option<Resolution>,
    /// Rate in Hz at which QEMU sends desktop updates, instead of the window's monitor's
    #[arg(long)]
    refresh: Option<f64>,
    /// Start in fullscreen
    #[arg(long)]
    fullscreen: bool,
    /// When the window holds the pointer: auto (while a game hides it), manual (Ctrl+Alt+G), never
    #[arg(long)]
    capture: Option<Capture>,
    /// Show the frame-rate overlay, and log it to stderr every second
    #[arg(long)]
    stats: bool,
}

pub async fn unblock<T>(f: impl FnOnce() -> T + Send + 'static) -> T
where
    T: Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(f());
    });
    rx.recv().await.expect("the worker thread sends its result")
}

// Only winit's device events carry the mouse's raw motion.
struct RawMotion;

impl CustomApplicationHandler for RawMotion {
    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) -> EventResult {
        if let DeviceEvent::MouseMotion { delta } = event {
            screen::raw_motion(delta.0, delta.1);
        }
        EventResult::Propagate
    }
}

fn window() -> Result<MainWindow> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .with_winit_custom_application_handler(RawMotion)
        .select()
        .context("no winit/OpenGL window backend")?;
    let ui = MainWindow::new()?;
    theme::follow(&ui);
    let weak = ui.as_weak();
    ui.on_toggle_fullscreen(move || {
        let Some(ui) = weak.upgrade() else { return };
        let on = !ui.window().is_fullscreen();
        ui.window().set_fullscreen(on);
        ui.set_fullscreen(on);
    });
    Ok(ui)
}

fn view(args: ViewArgs) -> Result<()> {
    let (qmp, display) = match (args.qmp, args.name) {
        (Some(qmp), _) => (qmp, config::Display::default()),
        (None, Some(name)) => {
            let cfg = config::load(&name)?;
            if run::running_pid(&name).is_none() {
                bail!("{name} is not running");
            }
            (RunPaths::new(&name).qmp(), cfg.display)
        },
        (None, None) => unreachable!("clap requires one"),
    };
    let ui = window()?;
    ui.set_manager(false);
    ui.set_stats_visible(args.stats);
    let opts = Options {
        resolution: args
            .resolution
            .or(display.resolution)
            .map(|r| (r.width, r.height)),
        refresh: args.refresh.or_else(|| display.refresh_hz.map(f64::from)),
        log_stats: args.stats,
        capture: args.capture.unwrap_or(display.capture),
    };
    let screen = Screen::install(&ui, opts);
    screen.borrow_mut().set_on_gone(|| {
        let _ = slint::quit_event_loop();
    });
    Screen::attach(&screen, qmp);
    if args.fullscreen {
        ui.invoke_toggle_fullscreen();
    }
    ui.run()?;
    Screen::detach(&screen);
    Ok(())
}

fn manage() -> Result<()> {
    let ui = window()?;
    let screen = Screen::install(&ui, Options::default());
    let _manager = Manager::install(&ui, screen.clone());
    ui.run()?;
    Screen::detach(&screen);
    Ok(())
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the command's output"
)]
fn list() {
    for cfg in config::load_all() {
        match cfg {
            Ok(c) => {
                let state = run::running_pid(&c.name)
                    .map_or_else(|| "stopped".into(), |pid| format!("running (pid {pid})"));
                let disk = c
                    .disks
                    .first()
                    .map(|d| d.source.path().display().to_string())
                    .unwrap_or_default();
                println!("{:20} {state:20} {disk}", c.name);
            },
            Err(e) => eprintln!("loophole: {e:#}"),
        }
    }
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the command's output"
)]
fn start(name: &str) -> Result<()> {
    let cfg = config::load(name)?;
    let mut tools = Tools::from_env()?;
    if cfg.media.driver_disc && tools.driver_disc.is_none() && !driver_disc::pinned() {
        let fetched = driver_disc::latest().and_then(|release| {
            eprintln!("loophole: downloading the driver disc, {}", release.build);
            driver_disc::download(&release, |_, _| {})
        });
        match fetched {
            Ok(disc) => tools.driver_disc = Some(disc.path),
            Err(e) => eprintln!("loophole: starting {name} without the driver disc: {e:#}"),
        }
    }
    run::start(&cfg, &tools)?;
    println!("{name} is running, `loophole view {name}` shows it");
    Ok(())
}

fn stop(name: &str, force: bool) -> Result<()> {
    config::load(name)?;
    if run::running_pid(name).is_none() {
        bail!("{name} is not running");
    }
    let cmd = if force {
        qmp::Command::Quit
    } else {
        qmp::Command::SystemPowerdown
    };
    Qmp::connect(&RunPaths::new(name).qmp())?.run(&cmd)?;
    Ok(())
}

fn check_disc(tools: &Tools) -> (bool, String) {
    if driver_disc::pinned() {
        return tools.driver_disc.as_ref().map_or_else(
            || (false, "LOOPHOLE_DRIVER_DISC names no file".into()),
            |d| (true, d.display().to_string()),
        );
    }
    let cached = driver_disc::cached();
    let detail = match (&cached, driver_disc::latest()) {
        (Some(c), Ok(r)) if c.build == r.build => {
            format!("{} (the latest), {}", c.build, c.path.display())
        },
        (Some(c), Ok(r)) => format!(
            "{}, {}; loophole downloads {} next",
            c.build,
            c.path.display(),
            r.build
        ),
        (Some(c), Err(e)) => format!("{}, {}; no update check: {e:#}", c.build, c.path.display()),
        (None, Ok(r)) => format!("not downloaded yet, loophole downloads {}", r.build),
        (None, Err(e)) => format!("not downloaded: {e:#}"),
    };
    (cached.is_some(), detail)
}

#[expect(clippy::print_stdout, reason = "the command's output")]
fn check(name: Option<&str>) -> ExitCode {
    let mut ready = true;
    let mut line = |ok: bool, required: bool, what: &str, detail: &str| {
        let mark = match (ok, required) {
            (true, _) => "ok",
            (false, true) => "MISSING",
            (false, false) => "missing",
        };
        ready &= ok || !required;
        println!("{mark:8} {what:12} {detail}");
    };
    for c in host::checks() {
        line(c.ok, c.required, c.what, &c.detail);
    }
    match Tools::from_env() {
        Ok(tools) => {
            line(true, true, "QEMU", &tools.qemu.display().to_string());
            let (ok, detail) = check_disc(&tools);
            line(ok, false, "Driver disc", &detail);
        },
        Err(e) => line(false, true, "QEMU", &format!("{e:#}")),
    }
    if let Some(name) = name {
        match config::load(name) {
            Ok(cfg) => {
                let issues: Vec<_> = cfg
                    .validate()
                    .into_iter()
                    .chain(run::preflight(&cfg))
                    .collect();
                if issues.is_empty() {
                    line(true, true, name, "ready to start");
                }
                for i in issues {
                    line(!i.fatal, true, name, &i.message);
                }
            },
            Err(e) => line(false, true, name, &format!("{e:#}")),
        }
    }
    if ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn main() -> Result<ExitCode> {
    match Cli::parse().command {
        None => manage()?,
        Some(Command::View(args)) => view(args)?,
        Some(Command::List) => list(),
        Some(Command::Start { name }) => start(&name)?,
        Some(Command::Stop { name, force }) => stop(&name, force)?,
        Some(Command::Check { name }) => return Ok(check(name.as_deref())),
    }
    Ok(ExitCode::SUCCESS)
}
