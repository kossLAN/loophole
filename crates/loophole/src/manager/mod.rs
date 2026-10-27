use std::cell::RefCell;
use std::process::Child;
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use loophole_vm::qmp::Command;
use loophole_vm::{Tools, VmConfig, host};
use slint::winit_030::{WinitWindowAccessor as _, winit};
use slint::{ComponentHandle as _, ModelRc, VecModel};

use crate::disc::DriverDisc;
use crate::editor::Editor;
use crate::screen::Screen;
use crate::ui::{AboutWindow, SettingsWindow, TrayIcon, TrayVm, ViewerWindow, VmListWindow};
use crate::{VmItem, models, resize, theme};

macro_rules! on {
    ($this:expr, $target:expr, $setter:ident, |$m:ident $(, $arg:ident)*| $body:expr) => {{
        let weak = std::rc::Rc::downgrade($this);
        $target.$setter(move |$($arg),*| {
            if let Some($m) = weak.upgrade() {
                $body
            }
        });
    }};
}

mod driver_disc;
mod settings;
mod viewers;
mod vms;

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

#[derive(Clone, PartialEq, Eq)]
enum Target {
    New,
    Vm(String),
}

enum Pending {
    Open(Target),
    Close,
}

struct Vm {
    cfg: VmConfig,
    state: State,
    control: Option<mpsc::Sender<Command>>,
    child: Option<Child>,
}

struct Viewer {
    name: String,
    ui: ViewerWindow,
    screen: Rc<RefCell<Screen>>,
    attached: bool,
}

pub struct Manager {
    list: VmListWindow,
    settings: SettingsWindow,
    about: AboutWindow,
    viewers: Vec<Viewer>,
    tray: TrayIcon,
    tray_vms: Rc<VecModel<TrayVm>>,
    tools: Result<Tools, String>,
    vms: Vec<Vm>,
    model: Rc<VecModel<VmItem>>,
    updates: async_channel::Sender<(String, Update)>,
    editor: Editor,
    on_edit: Rc<dyn Fn()>,
    pending: Option<Pending>,
    removing: Option<String>,
    disc: DriverDisc,
    disc_check: slint::Timer,
    fetching: bool,
}

const DISC_CHECK: Duration = Duration::from_hours(6);

fn summary(cfg: &VmConfig) -> String {
    format!("{} vCPU  {} GiB", cfg.cpu.vcpus(), cfg.memory.size_gib)
}

fn present(window: &slint::Window) {
    let _ = window.show();
    window.with_winit_window(winit::window::Window::focus_window);
}

impl Manager {
    pub fn install() -> Result<Rc<RefCell<Self>>> {
        let list = VmListWindow::new()?;
        let settings = SettingsWindow::new()?;
        let about = AboutWindow::new()?;
        let tray = TrayIcon::new()?;
        theme::follow(&list);
        theme::follow(&settings);
        theme::follow(&about);
        theme::follow_tray(&tray);
        resize::redraw_after_resize(&list);
        resize::redraw_after_resize(&settings);
        resize::redraw_after_resize(&about);
        about.set_version(env!("CARGO_PKG_VERSION").into());

        let (tx, rx) = async_channel::unbounded();
        let model = Rc::new(VecModel::default());
        list.set_vms(ModelRc::from(model.clone()));
        let tray_vms = Rc::new(VecModel::default());
        tray.set_vms(ModelRc::from(tray_vms.clone()));
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
            list.set_host_problem(format!("Missing: {}", problems.join("  ")).into());
        }

        let refresh = Rc::new(slint::Timer::default());
        let this = Rc::new_cyclic(|me: &Weak<RefCell<Self>>| {
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
            let editor = Editor::new(&settings, on_edit.clone());

            RefCell::new(Self {
                list: list.clone_strong(),
                settings: settings.clone_strong(),
                about: about.clone_strong(),
                viewers: Vec::new(),
                tray: tray.clone_strong(),
                tray_vms,
                tools,
                vms: Vec::new(),
                model,
                updates: tx,
                editor,
                on_edit,
                pending: None,
                removing: None,
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

        Self::wire_list(&this, &list);
        Self::wire_settings(&this, &settings);
        on!(&this, tray, on_open, |m| Self::show_list(&m));
        on!(&this, tray, on_view, |m, name| Self::open_viewer(&m, &name));
        on!(&this, tray, on_about, |m| Self::show_about(&m));
        tray.on_quit(|| {
            let _ = slint::quit_event_loop();
        });

        list.show()?;
        Ok(this)
    }

    fn show_list(this: &Rc<RefCell<Self>>) {
        let list = this.borrow().list.clone_strong();
        present(list.window());
    }

    fn show_about(this: &Rc<RefCell<Self>>) {
        let about = this.borrow().about.clone_strong();
        present(about.window());
    }

    fn vm(&self, name: &str) -> Option<&Vm> {
        self.vms.iter().find(|v| v.cfg.name == name)
    }

    fn message(&self, text: &str) {
        self.list.set_message(text.into());
    }

    fn vm_message(&self, name: &str, text: &str) {
        self.message(text);

        if let Some(v) = self.viewers.iter().find(|v| v.name == name) {
            v.ui.set_message(text.into());
        }
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
        let item = |name: &str| {
            items
                .iter()
                .find(|i| i.name == name)
                .cloned()
                .unwrap_or_default()
        };

        for v in &self.viewers {
            v.ui.set_vm(item(&v.name));
            v.ui.set_driver_disc_available(available);
        }

        if let Some(name) = self.editor.mode.name() {
            self.settings.set_vm(item(name));
        }

        models::sync(&self.model, items);
        self.update_tray();
    }

    fn update_tray(&self) {
        let items: Vec<TrayVm> = self
            .vms
            .iter()
            .map(|v| {
                let name = v.cfg.name.replace('_', "__");

                let label = if v.state == State::Stopped {
                    name
                } else {
                    format!("{name} ({})", v.state.as_str())
                };

                TrayVm {
                    name: v.cfg.name.as_str().into(),
                    label: label.into(),
                }
            })
            .collect();
        let running = self
            .vms
            .iter()
            .filter(|v| v.state != State::Stopped)
            .count();
        models::sync(&self.tray_vms, items);
        self.tray
            .set_running(i32::try_from(running).unwrap_or(i32::MAX));
    }

    fn sync(this: &Rc<RefCell<Self>>) {
        this.borrow_mut().sync_settings();
        Self::sync_viewers(this);
    }
}
