use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Child;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use ashpd::desktop::file_chooser::FileFilter;
use loophole_vm::argv::{DRIVER_DRIVE, INSTALL_DRIVE};
use loophole_vm::config;
use loophole_vm::qmp::{self, Command, Event, KeyValue, Qmp, StatusInfo};
use loophole_vm::{RunPaths, Tools, VmConfig, host, run};
use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, VecModel};

use crate::disc::{self, DriverDisc, Status};
use crate::editor::{Editor, Mode};
use crate::screen::{Options, Screen};
use crate::ui::{Edit, HostDevices};
use crate::{MainWindow, VmItem, picker, unblock};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Stopped,
    Starting,
    Running,
    Paused,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Paused => "paused",
        }
    }

    const fn running(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }
}

enum Update {
    State(State),
    Error(String),
}

enum Pending {
    Remove(String),
    Select(i32),
    NewVm,
}

struct Vm {
    cfg: VmConfig,
    state: State,
    control: Option<mpsc::Sender<Command>>,
    // QEMU, when this loophole started it. Reaped when it exits.
    child: Option<Child>,
}

pub struct Manager {
    ui: slint::Weak<MainWindow>,
    screen: Rc<RefCell<Screen>>,
    tools: Result<Tools, String>,
    vms: Vec<Vm>,
    model: Rc<VecModel<VmItem>>,
    updates: async_channel::Sender<(String, Update)>,
    attached: Option<String>,
    editor: Editor,
    on_edit: Rc<dyn Fn()>,
    pending: Option<Pending>,
    disc: DriverDisc,
    disc_check: slint::Timer,
    fetching: bool,
}

// A newer build of the driver disc is looked for at start and then this often.
const DISC_CHECK: Duration = Duration::from_hours(6);

fn monitor(
    name: String,
    updates: async_channel::Sender<(String, Update)>,
    commands: mpsc::Receiver<Command>,
) {
    let send = |u: Update| {
        let _ = updates.send_blocking((name.clone(), u));
    };
    let Ok(mut qmp) = Qmp::connect(&RunPaths::new(&name).control()) else {
        return send(Update::State(State::Stopped));
    };
    match qmp.execute::<StatusInfo>(&Command::QueryStatus) {
        Ok(s) if s.running => send(Update::State(State::Running)),
        Ok(_) => send(Update::State(State::Paused)),
        Err(_) => return send(Update::State(State::Stopped)),
    }
    loop {
        loop {
            match commands.try_recv() {
                Ok(cmd) => {
                    if let Err(e) = qmp.run(&cmd) {
                        send(Update::Error(format!("{e:#}")));
                    }
                },
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        match qmp.next_event(Duration::from_millis(200)) {
            Ok(Some(Event::Stop)) => send(Update::State(State::Paused)),
            Ok(Some(Event::Resume)) => send(Update::State(State::Running)),
            Ok(Some(Event::Other) | None) => {},
            Err(_) => return send(Update::State(State::Stopped)),
        }
    }
}

fn summary(cfg: &VmConfig) -> String {
    format!("{} vCPU · {} GiB", cfg.cpu.vcpus(), cfg.memory.size_gib)
}

fn screen_options(cfg: &VmConfig) -> Options {
    Options {
        resolution: cfg.display.resolution.map(|r| (r.width, r.height)),
        refresh: cfg.display.refresh_hz.map(f64::from),
        log_stats: false,
        capture: cfg.display.capture,
    }
}

impl Manager {
    pub fn install(ui: &MainWindow, screen: Rc<RefCell<Screen>>) -> Rc<RefCell<Self>> {
        let (tx, rx) = async_channel::unbounded();
        let model = Rc::new(VecModel::default());
        ui.set_vms(ModelRc::from(model.clone()));
        let tools = Tools::from_env().map_err(|e| format!("{e:#}"));
        let mut problems: Vec<String> = host::checks()
            .into_iter()
            .filter(|c| c.required && !c.ok)
            .map(|c| format!("{}: {}", c.what, c.detail))
            .collect();
        if let Err(e) = &tools {
            problems.push(e.clone());
        }
        if !problems.is_empty() {
            ui.set_host_problem(format!("Missing: {}", problems.join(" · ")).into());
        }

        let refresh = Rc::new(slint::Timer::default());
        let this = Rc::new_cyclic(|me: &std::rc::Weak<RefCell<Self>>| {
            let me = me.clone();
            let on_edit: Rc<dyn Fn()> = Rc::new(move || {
                let me = me.clone();
                refresh.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(120),
                    move || {
                        if let Some(m) = me.upgrade() {
                            m.borrow_mut().refresh_editor();
                        }
                    },
                );
            });
            RefCell::new(Self {
                ui: ui.as_weak(),
                screen: screen.clone(),
                tools,
                vms: Vec::new(),
                model,
                updates: tx,
                attached: None,
                editor: Editor::new(ui, on_edit.clone()),
                on_edit,
                pending: None,
                disc: DriverDisc::new(),
                disc_check: slint::Timer::default(),
                fetching: false,
            })
        });
        this.borrow_mut().reload(None);
        this.borrow().show_disc();
        Self::fetch_disc(&this);
        let weak = Rc::downgrade(&this);
        this.borrow()
            .disc_check
            .start(slint::TimerMode::Repeated, DISC_CHECK, move || {
                if let Some(this) = weak.upgrade() {
                    Self::fetch_disc(&this);
                }
            });

        let weak = Rc::downgrade(&this);
        let _ = slint::spawn_local(async move {
            while let Ok((name, update)) = rx.recv().await {
                let Some(this) = weak.upgrade() else { return };
                Self::update(&this, &name, update);
            }
        });

        let weak = Rc::downgrade(&this);
        screen.borrow_mut().set_on_gone(move || {
            if let Some(this) = weak.upgrade() {
                this.borrow_mut().attached = None;
            }
        });

        macro_rules! on {
            ($target:expr, $setter:ident, |$m:ident $(, $arg:ident)*| $body:expr) => {{
                let weak = Rc::downgrade(&this);
                $target.$setter(move |$($arg),*| {
                    if let Some($m) = weak.upgrade() {
                        $body
                    }
                });
            }};
        }
        on!(ui, on_select, |m, i| Self::request(&m, Pending::Select(i)));
        on!(ui, on_new_vm, |m| Self::request(&m, Pending::NewVm));
        on!(ui, on_start, |m| Self::start(&m));
        on!(ui, on_shutdown, |m| m
            .borrow()
            .command(Command::SystemPowerdown));
        on!(ui, on_force_off, |m| m.borrow().command(Command::Quit));
        on!(ui, on_pause, |m| m.borrow().command(Command::Stop));
        on!(ui, on_resume, |m| m.borrow().command(Command::Cont));
        on!(ui, on_reset, |m| m.borrow().command(Command::SystemReset));
        on!(ui, on_ctrl_alt_del, |m| m.borrow().command(
            Command::SendKey {
                keys: ["ctrl", "alt", "delete"].map(KeyValue::Qcode).into(),
            }
        ));
        on!(ui, on_eject_install_media, |m| Self::change_media(
            &m,
            |cfg, _| {
                cfg.media.install = None;
                Some((INSTALL_DRIVE, None))
            }
        ));
        on!(ui, on_set_driver_disc, |m, on| Self::change_media(
            &m,
            |cfg, tools| {
                let disc = tools.driver_disc.clone()?;
                cfg.media.driver_disc = on;
                Some((DRIVER_DRIVE, on.then_some(disc)))
            }
        ));
        on!(ui, on_remove_vm, |m| m.borrow_mut().confirm_remove());
        on!(ui, on_confirmed, |m| Self::confirmed(&m));
        on!(ui, on_settings_view_changed, |m| Self::show_selected(&m));

        let edit = ui.global::<Edit>();
        let on_edit = this.borrow().on_edit.clone();
        edit.on_edited(move || on_edit());
        on!(edit, on_apply, |m| Self::apply(&m));
        on!(edit, on_revert, |m| {
            let mut m = m.borrow_mut();
            m.editor.revert();
            m.refresh_editor();
        });
        on!(edit, on_cancel_create, |m| {
            m.borrow_mut().editor.hide();
            Self::show_selected(&m);
        });
        on!(edit, on_browse_install, |m| Self::browse(
            &m,
            "Windows installation image",
            FileFilter::new("ISO images").glob("*.iso").glob("*.ISO"),
            None,
        ));
        on!(edit, on_browse_disk, |m, i| Self::browse(
            &m,
            "Disk image",
            FileFilter::new("Disk images")
                .glob("*.qcow2")
                .glob("*.img")
                .glob("*.raw"),
            usize::try_from(i).ok(),
        ));
        on!(edit, on_add_disk, |m, kind| m
            .borrow_mut()
            .editor
            .add_disk(kind, None));
        on!(edit, on_remove_disk, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.remove_disk(i);
            }
        });
        on!(edit, on_add_nic, |m| m.borrow().editor.add_nic());
        on!(edit, on_remove_nic, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.remove_nic(i);
            }
        });
        on!(edit, on_new_mac, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.new_mac(i);
            }
        });
        on!(edit, on_add_forward, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.add_forward(i);
            }
        });
        on!(edit, on_remove_forward, |m, i, j| {
            if let (Ok(i), Ok(j)) = (usize::try_from(i), usize::try_from(j)) {
                m.borrow().editor.remove_forward(i, j);
            }
        });
        on!(edit, on_remove_pci, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.remove_pci(i);
            }
        });
        on!(edit, on_remove_usb, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.remove_usb(i);
            }
        });
        on!(edit, on_move_boot, |m, i, delta| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.move_boot(i, delta);
            }
        });
        on!(edit, on_pick_device, |m, target| Self::pick_device(
            &m, target
        ));

        let devices = ui.global::<HostDevices>();
        on!(devices, on_scan, |m| m.borrow().editor.scan_host());
        on!(devices, on_add_disk, |m, path| {
            let mut m = m.borrow_mut();
            let path = PathBuf::from(path.as_str());
            match m.editor.pick_disk.take() {
                Some(i) => m.editor.set_disk(i, 2, &path),
                None => m.editor.add_disk(2, Some(path)),
            }
        });
        on!(devices, on_add_pci, |m, address| m
            .borrow_mut()
            .editor
            .add_pci(&address));
        on!(devices, on_add_usb, |m, id| m.borrow().editor.add_usb(&id));

        Self::show_selected(&this);
        this
    }

    fn selected_index(&self) -> Option<usize> {
        let ui = self.ui.upgrade()?;
        usize::try_from(ui.get_selected())
            .ok()
            .filter(|&i| i < self.vms.len())
    }

    fn selected(&self) -> Option<&Vm> {
        self.selected_index().map(|i| &self.vms[i])
    }

    fn message(&self, text: &str) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_message(text.into());
        }
    }

    fn others(&self) -> Vec<String> {
        self.vms.iter().map(|v| v.cfg.name.clone()).collect()
    }

    fn refresh_editor(&mut self) {
        let others = self.others();
        let tools = self.tools.as_ref().ok().cloned();
        self.editor.refresh(&others, tools.as_ref());
    }

    fn reload(&mut self, select: Option<&str>) {
        let previous = select
            .map(String::from)
            .or_else(|| self.selected().map(|v| v.cfg.name.clone()));
        let mut old: Vec<Vm> = std::mem::take(&mut self.vms);
        let mut errors = Vec::new();
        for cfg in config::load_all() {
            let cfg = match cfg {
                Ok(c) => c,
                Err(e) => {
                    errors.push(format!("{e:#}"));
                    continue;
                },
            };
            let vm = if let Some(i) = old.iter().position(|v| v.cfg.name == cfg.name) {
                let mut v = old.remove(i);
                v.cfg = cfg;
                v
            } else {
                let running = run::running_pid(&cfg.name).is_some();
                let mut v = Vm {
                    cfg,
                    state: State::Stopped,
                    control: None,
                    child: None,
                };
                if running {
                    v.state = State::Running;
                    v.control = Some(self.spawn_monitor(&v.cfg.name));
                }
                v
            };
            self.vms.push(vm);
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let index = previous
            .and_then(|n| self.vms.iter().position(|v| v.cfg.name == n))
            .or_else(|| (!self.vms.is_empty()).then_some(0))
            .map_or(-1, |i| i32::try_from(i).unwrap_or(-1));
        ui.set_selected(index);
        if !errors.is_empty() {
            ui.set_message(errors.join(" · ").into());
        }
        self.refresh_model();
    }

    fn spawn_monitor(&self, name: &str) -> mpsc::Sender<Command> {
        let (tx, rx) = mpsc::channel();
        let (name, updates) = (name.to_owned(), self.updates.clone());
        std::thread::spawn(move || monitor(name, updates, rx));
        tx
    }

    fn refresh_model(&self) {
        let available = self.driver_disc_available();
        let items: Vec<VmItem> = self
            .vms
            .iter()
            .map(|v| VmItem {
                name: v.cfg.name.as_str().into(),
                windows: v.cfg.guest.windows.as_str().into(),
                state: v.state.as_str().into(),
                summary: summary(&v.cfg).into(),
                running: v.state.running(),
                paused: v.state == State::Paused,
                install_media: v.cfg.media.install.is_some(),
                driver_disc: available && v.cfg.media.driver_disc,
            })
            .collect();
        if self.model.row_count() == items.len() {
            for (i, item) in items.into_iter().enumerate() {
                if self.model.row_data(i).as_ref() != Some(&item) {
                    self.model.set_row_data(i, item);
                }
            }
        } else {
            self.model.set_vec(items);
        }
    }

    fn update(this: &Rc<RefCell<Self>>, name: &str, update: Update) {
        {
            let mut m = this.borrow_mut();
            let Some(vm) = m.vms.iter_mut().find(|v| v.cfg.name == name) else {
                return;
            };
            match update {
                Update::State(state) => {
                    vm.state = state;
                    if state == State::Stopped {
                        vm.control = None;
                        if let Some(mut child) = vm.child.take() {
                            let _ = child.wait();
                        }
                    }
                },
                Update::Error(e) => m.message(&format!("{name}: {e}")),
            }
            m.refresh_model();
        }
        Self::show_selected(this);
    }

    // The selected VM's screen while it runs, its settings while it does not or when asked for.
    fn show_selected(this: &Rc<RefCell<Self>>) {
        let (attach, detach) = {
            let mut m = this.borrow_mut();
            let Some(ui) = m.ui.upgrade() else { return };
            let selected = m.selected().map(|v| (v.cfg.clone(), v.state));
            let running = selected.as_ref().is_some_and(|(_, s)| s.running());
            if !running {
                ui.set_settings_view(false);
            }
            let creating = m.editor.mode == Mode::Create;
            ui.set_show_screen(running && !creating && !ui.get_settings_view());

            if !creating {
                match &selected {
                    Some((cfg, state)) => {
                        let mode = if *state == State::Stopped {
                            Mode::Edit(cfg.name.clone())
                        } else {
                            Mode::View(cfg.name.clone())
                        };
                        let keep = m.editor.dirty && m.editor.mode.name() == mode.name();
                        if m.editor.mode != mode && !keep {
                            m.editor.load(mode, cfg);
                        }
                        m.set_log(cfg, *state);
                    },
                    None => m.editor.hide(),
                }
                m.refresh_editor();
            }

            let want = selected.filter(|_| running).map(|(c, _)| c);
            let want_name = want.as_ref().map(|c| c.name.clone());
            if want_name == m.attached {
                (None, false)
            } else {
                let detach = m.attached.is_some();
                m.attached.clone_from(&want_name);
                (want, detach)
            }
        };
        let screen = this.borrow().screen.clone();
        if detach {
            Screen::detach(&screen);
        }
        if let Some(cfg) = attach {
            screen.borrow_mut().set_options(screen_options(&cfg));
            Screen::attach(&screen, RunPaths::new(&cfg.name).qmp());
        }
        screen.borrow().update_hint();
    }

    fn driver_disc_available(&self) -> bool {
        self.tools.as_ref().is_ok_and(|t| t.driver_disc.is_some())
    }

    fn show_disc(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let available = self.driver_disc_available();
        let edit = ui.global::<Edit>();
        edit.set_driver_disc_available(available);
        edit.set_driver_disc_name(self.disc.name().into());
        edit.set_driver_disc_hint(self.disc.hint(available).into());
    }

    // A VM that runs keeps the disc it has until the disc menu changes it.
    fn fetch_disc(this: &Rc<RefCell<Self>>) {
        let have = {
            let mut m = this.borrow_mut();
            if m.disc.status == Status::Pinned || m.fetching {
                return;
            }
            m.fetching = true;
            m.disc.build.clone()
        };
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            disc::fetch(have.as_deref(), |u| {
                let _ = tx.send_blocking(u);
            });
        });
        let weak = Rc::downgrade(this);
        let _ = slint::spawn_local(async move {
            while let Ok(update) = rx.recv().await {
                let Some(this) = weak.upgrade() else { return };
                this.borrow_mut().disc_update(update);
            }
            if let Some(this) = weak.upgrade() {
                this.borrow_mut().fetching = false;
            }
        });
    }

    fn disc_update(&mut self, update: disc::Update) {
        match update {
            disc::Update::Current => self.disc.status = Status::Current,
            disc::Update::Downloading(build, percent) => {
                self.disc.status = Status::Downloading(build, percent);
            },
            disc::Update::Failed(why) => self.disc.status = Status::Failed(why),
            disc::Update::Done(d) => {
                self.disc.build = Some(d.build);
                self.disc.status = Status::Current;
                if let Ok(tools) = &mut self.tools {
                    tools.driver_disc = Some(d.path);
                }
                self.show_disc();
                self.refresh_model();
                self.refresh_editor();
                return;
            },
        }
        self.show_disc();
    }

    fn set_log(&self, cfg: &VmConfig, state: State) {
        let Some(ui) = self.ui.upgrade() else { return };
        ui.global::<Edit>().set_log(if state == State::Stopped {
            run::log_tail(&cfg.name, 15).into()
        } else {
            SharedString::new()
        });
    }

    fn command(&self, cmd: Command) {
        if let Some(tx) = self.selected().and_then(|v| v.control.as_ref()) {
            let _ = tx.send(cmd);
        }
    }

    // `change` edits the definition and names the drive and the medium to put in it, which a
    // running VM gets at once.
    fn change_media(
        this: &Rc<RefCell<Self>>,
        change: impl FnOnce(&mut VmConfig, &Tools) -> Option<(&'static str, Option<PathBuf>)>,
    ) {
        {
            let mut m = this.borrow_mut();
            let Ok(tools) = m.tools.clone() else { return };
            let Some(i) = m.selected_index() else { return };
            if m.vms[i].state == State::Starting {
                return;
            }
            let mut cfg = m.vms[i].cfg.clone();
            let Some((drive, medium)) = change(&mut cfg, &tools) else {
                return;
            };
            if let Err(e) = config::save(&cfg) {
                m.message(&format!("{}: {e:#}", cfg.name));
                return;
            }
            if let Some(tx) = &m.vms[i].control {
                for cmd in qmp::change_medium(drive, medium) {
                    let _ = tx.send(cmd);
                }
            }
            m.vms[i].cfg = cfg.clone();
            m.refresh_model();
            if !m.editor.dirty && m.editor.mode.name() == Some(cfg.name.as_str()) {
                let mode = m.editor.mode.clone();
                m.editor.load(mode, &cfg);
            }
        }
        Self::show_selected(this);
    }

    fn start(this: &Rc<RefCell<Self>>) {
        let (cfg, tools) = {
            let mut m = this.borrow_mut();
            let tools = match &m.tools {
                Ok(t) => t.clone(),
                Err(e) => {
                    m.message(&format!("Cannot start VMs: {e}"));
                    return;
                },
            };
            let Some(i) = m.selected_index() else { return };
            if m.vms[i].state != State::Stopped || m.editor.dirty {
                return;
            }
            m.vms[i].state = State::Starting;
            let cfg = m.vms[i].cfg.clone();
            m.refresh_model();
            m.message("");
            (cfg, tools)
        };
        Self::show_selected(this);
        let weak = Rc::downgrade(this);
        let _ = slint::spawn_local(async move {
            let started = {
                let cfg = cfg.clone();
                unblock(move || run::start(&cfg, &tools)).await
            };
            let Some(this) = weak.upgrade() else { return };
            {
                let mut m = this.borrow_mut();
                let control = started.is_ok().then(|| m.spawn_monitor(&cfg.name));
                let Some(i) = m.vms.iter().position(|v| v.cfg.name == cfg.name) else {
                    return;
                };
                match started {
                    Ok(child) => {
                        let vm = &mut m.vms[i];
                        vm.child = Some(child);
                        vm.control = control;
                        vm.state = State::Running;
                    },
                    Err(e) => {
                        m.vms[i].state = State::Stopped;
                        m.message(&format!(
                            "{}: {}",
                            cfg.name,
                            e.to_string().lines().next().unwrap_or("")
                        ));
                    },
                }
                m.refresh_model();
            }
            Self::show_selected(&this);
        });
    }

    // Leaving unapplied changes asks first.
    fn request(this: &Rc<RefCell<Self>>, next: Pending) {
        let ask = {
            let m = this.borrow();
            let reselect = match &next {
                Pending::Select(i) => {
                    m.editor.mode != Mode::Create
                        && m.ui.upgrade().is_some_and(|ui| ui.get_selected() == *i)
                },
                Pending::Remove(_) | Pending::NewVm => false,
            };
            m.editor.dirty && !reselect
        };
        if !ask {
            this.borrow_mut().pending = Some(next);
            Self::confirmed(this);
            return;
        }
        let mut m = this.borrow_mut();
        let Some(ui) = m.ui.upgrade() else { return };
        let what = match &m.editor.mode {
            Mode::Create => "the new VM".to_owned(),
            mode => mode.name().unwrap_or_default().to_owned(),
        };
        ui.set_confirm_title(format!("Discard changes to {what}?").into());
        ui.set_confirm_text("They have not been applied.".into());
        ui.set_confirm_action("Discard".into());
        ui.set_confirm_destructive(true);
        ui.set_confirm_open(true);
        m.pending = Some(next);
    }

    fn confirmed(this: &Rc<RefCell<Self>>) {
        let pending = {
            let mut m = this.borrow_mut();
            if let Some(ui) = m.ui.upgrade() {
                ui.set_confirm_open(false);
            }
            m.pending.take()
        };
        match pending {
            Some(Pending::Remove(name)) => this.borrow_mut().remove(&name),
            Some(Pending::Select(i)) => {
                let mut m = this.borrow_mut();
                if let Some(ui) = m.ui.upgrade() {
                    ui.set_selected(i);
                    ui.set_message(SharedString::new());
                    ui.set_settings_view(false);
                }
                m.editor.hide();
            },
            Some(Pending::NewVm) => {
                let mut m = this.borrow_mut();
                m.editor.create();
                m.refresh_editor();
                m.message("");
            },
            None => return,
        }
        Self::show_selected(this);
    }

    fn apply(this: &Rc<RefCell<Self>>) {
        let result = {
            let mut m = this.borrow_mut();
            let draft = m.editor.draft(&m.others());
            if draft.fatal().next().is_some() {
                m.editor.show_problem(&draft);
                m.refresh_editor();
                return;
            }
            (|| -> anyhow::Result<VmConfig> {
                if !draft.new_images.is_empty() {
                    let tools = m
                        .tools
                        .as_ref()
                        .map_err(|e| anyhow::anyhow!("cannot create disks: {e}"))?;
                    for &(i, gib) in &draft.new_images {
                        run::create_disk(tools, draft.cfg.disks[i].source.path(), gib)?;
                    }
                }
                config::save(&draft.cfg)?;
                Ok(draft.cfg)
            })()
        };
        let mut m = this.borrow_mut();
        match result {
            Ok(cfg) => {
                m.reload(Some(&cfg.name));
                let state = m
                    .vms
                    .iter()
                    .find(|v| v.cfg.name == cfg.name)
                    .map_or(State::Stopped, |v| v.state);
                let mode = if state == State::Stopped {
                    Mode::Edit(cfg.name.clone())
                } else {
                    Mode::View(cfg.name.clone())
                };
                m.editor.load(mode, &cfg);
                m.message("");
                drop(m);
                Self::show_selected(this);
            },
            Err(e) => {
                m.message(&format!("{e:#}"));
                m.refresh_editor();
            },
        }
    }

    // `disk`: the editor's disk row to fill. None fills the installation image.
    fn browse(
        this: &Rc<RefCell<Self>>,
        title: &'static str,
        filter: FileFilter,
        disk: Option<usize>,
    ) {
        let (ui, generation) = {
            let m = this.borrow();
            (m.ui.clone(), m.editor.generation)
        };
        let weak = Rc::downgrade(this);
        let _ = slint::spawn_local(async move {
            let picked = picker::open_file(title, filter).await;
            let (Some(this), Some(ui)) = (weak.upgrade(), ui.upgrade()) else {
                return;
            };
            let m = this.borrow();
            if m.editor.generation != generation {
                return;
            }
            match picked {
                Ok(Some(path)) => match disk {
                    Some(i) => m.editor.set_disk(i, 1, &path),
                    None => m.editor.set_install(&path),
                },
                Ok(None) => {},
                Err(e) => ui.set_message(format!("{e:#}").into()),
            }
        });
    }

    // `target`: a disk row to fill, -1 for a new host-disk row, -2 PCI, -3 USB.
    fn pick_device(this: &Rc<RefCell<Self>>, target: i32) {
        let mut m = this.borrow_mut();
        let Some(ui) = m.ui.upgrade() else { return };
        m.editor.scan_host();
        let kind = match target {
            -2 => 2,
            -3 => 3,
            t => {
                m.editor.pick_disk = usize::try_from(t).ok();
                1
            },
        };
        ui.global::<HostDevices>().set_picking(kind);
    }

    fn confirm_remove(&mut self) {
        let Some(vm) = self.selected().filter(|v| v.state == State::Stopped) else {
            return;
        };
        let cfg = &vm.cfg;
        let owned = cfg.owned_images();
        let kept: Vec<String> = cfg
            .disks
            .iter()
            .map(|d| d.source.path())
            .filter(|p| !owned.contains(p))
            .map(|p| p.display().to_string())
            .collect();
        let mut text = if owned.is_empty() {
            "This deletes its UEFI variables and its TPM.".to_owned()
        } else {
            let owned: Vec<String> = owned.iter().map(|p| p.display().to_string()).collect();
            format!(
                "This deletes {} with everything installed on it, its UEFI variables and its TPM.",
                owned.join(", ")
            )
        };
        if !kept.is_empty() {
            text += &format!(" {} stays.", kept.join(", "));
        }
        let Some(ui) = self.ui.upgrade() else { return };
        ui.set_confirm_title(format!("Remove {}?", cfg.name).into());
        ui.set_confirm_text(text.into());
        ui.set_confirm_action("Remove".into());
        ui.set_confirm_destructive(true);
        ui.set_confirm_open(true);
        self.pending = Some(Pending::Remove(cfg.name.clone()));
    }

    fn remove(&mut self, name: &str) {
        let Some(vm) = self
            .vms
            .iter()
            .find(|v| v.cfg.name == name && v.state == State::Stopped)
        else {
            return;
        };
        match config::remove(&vm.cfg) {
            Ok(()) => {
                self.message(&format!("Removed {name}."));
                self.editor.hide();
                self.reload(None);
            },
            Err(e) => self.message(&format!("{name}: {e:#}")),
        }
    }
}
