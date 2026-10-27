use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use loophole_vm::config;
use loophole_vm::qmp::{self, Command, Event, Qmp, StatusInfo};
use loophole_vm::{RunPaths, Tools, VmConfig, run};
use slint::ComponentHandle as _;

use super::{Manager, State, Target, Update, Vm};
use crate::ui::VmListWindow;
use crate::unblock;

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

impl Manager {
    pub(super) fn wire_list(this: &Rc<RefCell<Self>>, list: &VmListWindow) {
        macro_rules! on_selected {
            ($setter:ident, |$m:ident, $name:ident| $body:expr) => {
                on!(this, list, $setter, |$m| {
                    let selected = $m.borrow().selected_name();
                    if let Some($name) = selected {
                        $body
                    }
                })
            };
        }
        on!(this, list, on_new_vm, |m| Self::open_settings(
            &m,
            Target::New
        ));
        on_selected!(on_start_vm, |m, name| Self::start(&m, &name));
        on_selected!(on_shutdown, |m, name| m
            .borrow()
            .command(&name, Command::SystemPowerdown));
        on_selected!(on_force_off, |m, name| m
            .borrow()
            .command(&name, Command::Quit));
        on_selected!(on_open_viewer, |m, name| Self::open_viewer(&m, &name));
        on_selected!(on_open_settings, |m, name| Self::open_settings(
            &m,
            Target::Vm(name)
        ));
        on_selected!(on_remove_vm, |m, name| m.borrow_mut().confirm_remove(&name));
        on!(this, list, on_about, |m| Self::show_about(&m));
        on!(this, list, on_confirmed, |m| {
            let mut m = m.borrow_mut();
            m.list.set_confirm_open(false);

            if let Some(name) = m.removing.take() {
                m.remove(&name);
            }
        });
    }

    fn selected_name(&self) -> Option<String> {
        usize::try_from(self.list.get_selected())
            .ok()
            .and_then(|i| self.vms.get(i))
            .map(|v| v.cfg.name.clone())
    }

    pub(super) fn reload(&mut self, select: Option<&str>) {
        let previous = select.map(String::from).or_else(|| self.selected_name());
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

        let index = previous
            .and_then(|n| self.vms.iter().position(|v| v.cfg.name == n))
            .or_else(|| (!self.vms.is_empty()).then_some(0))
            .map_or(-1, |i| i32::try_from(i).unwrap_or(-1));
        self.list.set_selected(index);

        if !errors.is_empty() {
            self.message(&errors.join("  "));
        }

        self.refresh_model();
    }

    fn spawn_monitor(&self, name: &str) -> mpsc::Sender<Command> {
        let (tx, rx) = mpsc::channel();
        let (name, updates) = (name.to_owned(), self.updates.clone());
        std::thread::spawn(move || monitor(name, updates, rx));
        tx
    }

    pub(super) fn update(this: &Rc<RefCell<Self>>, name: &str, update: Update) {
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
                            std::thread::spawn(move || child.wait());
                        }
                    }
                },
                Update::Error(e) => m.vm_message(name, &format!("{name}: {e}")),
            }

            m.refresh_model();
        }
        Self::sync(this);
    }

    pub(super) fn command(&self, name: &str, cmd: Command) {
        if let Some(tx) = self.vm(name).and_then(|v| v.control.as_ref()) {
            let _ = tx.send(cmd);
        }
    }

    pub(super) fn change_media(
        this: &Rc<RefCell<Self>>,
        name: &str,
        change: impl FnOnce(&mut VmConfig, &Tools) -> Option<(&'static str, Option<PathBuf>)>,
    ) {
        {
            let mut m = this.borrow_mut();
            let Ok(tools) = m.tools.clone() else { return };

            let Some(i) = m.vms.iter().position(|v| v.cfg.name == name) else {
                return;
            };

            if m.vms[i].state == State::Starting {
                return;
            }

            let mut cfg = m.vms[i].cfg.clone();

            let Some((drive, medium)) = change(&mut cfg, &tools) else {
                return;
            };

            if let Err(e) = config::save(&cfg) {
                m.vm_message(name, &format!("{name}: {e:#}"));
                return;
            }

            if let Some(tx) = &m.vms[i].control {
                for cmd in qmp::change_medium(drive, medium) {
                    let _ = tx.send(cmd);
                }
            }

            m.vms[i].cfg = cfg.clone();
            m.refresh_model();

            if !m.editor.dirty && m.editor.mode.name() == Some(name) {
                let mode = m.editor.mode.clone();
                m.editor.load(mode, &cfg);
            }
        }
        Self::sync(this);
    }

    pub(super) fn start(this: &Rc<RefCell<Self>>, name: &str) {
        let (cfg, tools) = {
            let mut m = this.borrow_mut();

            let tools = match &m.tools {
                Ok(t) => t.clone(),
                Err(e) => {
                    m.vm_message(name, &format!("Cannot start VMs: {e}"));
                    return;
                },
            };

            let Some(i) = m.vms.iter().position(|v| v.cfg.name == name) else {
                return;
            };

            if m.vms[i].state != State::Stopped {
                return;
            }

            if m.editor.dirty && m.editor.mode.name() == Some(name) {
                m.vm_message(name, &format!("{name}: apply or revert its changes first"));
                return;
            }

            m.vms[i].state = State::Starting;
            let cfg = m.vms[i].cfg.clone();
            m.refresh_model();
            m.vm_message(name, "");
            (cfg, tools)
        };
        Self::sync(this);
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
                        m.vm_message(
                            &cfg.name,
                            &format!(
                                "{}: {}",
                                cfg.name,
                                e.to_string().lines().next().unwrap_or("")
                            ),
                        );
                    },
                }

                m.refresh_model();
            }
            Self::sync(&this);
        });
    }

    fn confirm_remove(&mut self, name: &str) {
        let Some(vm) = self.vm(name).filter(|v| v.state == State::Stopped) else {
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

        self.list
            .set_confirm_title(format!("Remove {}?", cfg.name).into());
        self.list.set_confirm_text(text.into());
        self.list.set_confirm_action("Remove".into());
        self.list.set_confirm_open(true);
        self.removing = Some(cfg.name.clone());
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

                if self.editor.mode.name() == Some(name) {
                    self.editor.hide();
                    let _ = self.settings.hide();
                }

                self.close_viewer(name);
                self.reload(None);
            },
            Err(e) => self.message(&format!("{name}: {e:#}")),
        }
    }
}
