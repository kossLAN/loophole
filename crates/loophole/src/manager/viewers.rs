use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use loophole_vm::argv::{DRIVER_DRIVE, INSTALL_DRIVE};
use loophole_vm::qmp::{Command, KeyValue};
use loophole_vm::{RunPaths, VmConfig};
use slint::{CloseRequestResponse, ComponentHandle as _};

use super::{Manager, State, Target, Viewer, present};
use crate::screen::{Options, Screen};
use crate::viewer_window;

fn screen_options(cfg: &VmConfig) -> Options {
    Options {
        resolution: cfg.display.resolution.map(|r| (r.width, r.height)),
        refresh: cfg.display.refresh_hz.map(f64::from),
        log_stats: false,
        capture: cfg.display.capture,
    }
}

fn close(viewer: Viewer) {
    Screen::detach(&viewer.screen);
    slint::Timer::single_shot(Duration::ZERO, move || drop(viewer));
}

impl Manager {
    pub fn close_viewers(&self) {
        for v in &self.viewers {
            Screen::detach(&v.screen);
        }
    }

    pub(super) fn open_viewer(this: &Rc<RefCell<Self>>, name: &str) {
        let shown = {
            let m = this.borrow();

            if m.vm(name).is_none() {
                return;
            }

            m.viewers
                .iter()
                .find(|v| v.name == name)
                .map(|v| v.ui.clone_strong())
        };

        if let Some(ui) = shown {
            present(ui.window());
            return;
        }

        let ui = match viewer_window() {
            Ok(ui) => ui,
            Err(e) => {
                this.borrow().message(&format!("{e:#}"));
                return;
            },
        };

        ui.set_vm_name(name.into());
        let screen = Screen::install(&ui, Options::default());

        let weak = Rc::downgrade(this);
        let owned = name.to_owned();
        screen.borrow_mut().set_on_gone(move || {
            let Some(m) = weak.upgrade() else { return };

            if let Some(v) = m.borrow_mut().viewers.iter_mut().find(|v| v.name == owned) {
                v.attached = false;
            }

            let weak = weak.clone();
            slint::Timer::single_shot(Duration::from_secs(1), move || {
                if let Some(m) = weak.upgrade() {
                    Self::sync(&m);
                }
            });
        });

        macro_rules! on_vm {
            ($setter:ident, |$m:ident, $vm:ident $(, $arg:ident)*| $body:expr) => {{
                let owned = name.to_owned();
                on!(this, ui, $setter, |$m $(, $arg)*| {
                    let $vm = owned.as_str();
                    $body
                });
            }};
        }
        on_vm!(on_start, |m, vm| Self::start(&m, vm));
        on_vm!(on_shutdown, |m, vm| m
            .borrow()
            .command(vm, Command::SystemPowerdown));
        on_vm!(on_force_off, |m, vm| m.borrow().command(vm, Command::Quit));
        on_vm!(on_pause, |m, vm| m.borrow().command(vm, Command::Stop));
        on_vm!(on_resume, |m, vm| m.borrow().command(vm, Command::Cont));
        on_vm!(on_reset, |m, vm| m
            .borrow()
            .command(vm, Command::SystemReset));
        on_vm!(on_ctrl_alt_del, |m, vm| m.borrow().command(
            vm,
            Command::SendKey {
                keys: ["ctrl", "alt", "delete"].map(KeyValue::Qcode).into(),
            }
        ));
        on_vm!(on_eject_install_media, |m, vm| Self::change_media(
            &m,
            vm,
            |cfg, _| {
                cfg.media.install = None;
                Some((INSTALL_DRIVE, None))
            }
        ));
        on_vm!(on_set_driver_disc, |m, vm, on| Self::change_media(
            &m,
            vm,
            |cfg, tools| {
                let disc = tools.driver_disc.clone()?;
                cfg.media.driver_disc = on;
                Some((DRIVER_DRIVE, on.then_some(disc)))
            }
        ));
        on_vm!(on_settings, |m, vm| Self::open_settings(
            &m,
            Target::Vm(vm.to_owned())
        ));

        let weak = Rc::downgrade(this);
        let owned = name.to_owned();
        ui.window().on_close_requested(move || {
            if let Some(m) = weak.upgrade() {
                let viewer = m.borrow_mut().take_viewer(&owned);

                if let Some(viewer) = viewer {
                    close(viewer);
                }
            }

            CloseRequestResponse::HideWindow
        });

        {
            let mut m = this.borrow_mut();
            m.viewers.push(Viewer {
                name: name.to_owned(),
                ui: ui.clone_strong(),
                screen,
                attached: false,
            });
            m.refresh_model();
        }
        present(ui.window());
        Self::sync(this);
    }

    fn take_viewer(&mut self, name: &str) -> Option<Viewer> {
        let i = self.viewers.iter().position(|v| v.name == name)?;
        Some(self.viewers.remove(i))
    }

    pub(super) fn close_viewer(&mut self, name: &str) {
        if let Some(viewer) = self.take_viewer(name) {
            let _ = viewer.ui.hide();
            close(viewer);
        }
    }

    pub(super) fn sync_viewers(this: &Rc<RefCell<Self>>) {
        let mut attach = Vec::new();
        let mut detach = Vec::new();
        {
            let mut m = this.borrow_mut();
            let m = &mut *m;

            for v in &mut m.viewers {
                let vm = m.vms.iter().find(|vm| vm.cfg.name == v.name);
                let state = vm.map_or(State::Stopped, |vm| vm.state);

                if state.running() && !v.attached {
                    if let Some(vm) = vm {
                        attach.push((v.screen.clone(), vm.cfg.clone()));
                    }
                } else if !state.running() && v.attached {
                    detach.push(v.screen.clone());
                }

                v.attached = state.running();

                if !v.attached {
                    v.ui.set_placeholder(
                        if state == State::Starting {
                            format!("Starting {}…", v.name)
                        } else {
                            format!("{} is not running", v.name)
                        }
                        .into(),
                    );
                }
            }
        }

        for screen in detach {
            Screen::detach(&screen);
        }

        for (screen, cfg) in attach {
            let run = RunPaths::new(&cfg.name);
            screen.borrow_mut().set_options(screen_options(&cfg));
            Screen::attach(
                &screen,
                run.qmp(),
                cfg.display.clipboard.then(|| run.clipboard()),
            );
        }
    }
}
