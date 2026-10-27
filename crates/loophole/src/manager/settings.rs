use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use ashpd::desktop::file_chooser::FileFilter;
use loophole_vm::{VmConfig, config, run};
use slint::{CloseRequestResponse, ComponentHandle as _, SharedString};

use super::{Manager, Pending, State, Target, present};
use crate::editor::Mode;
use crate::picker;
use crate::ui::{DiskKind, Edit, HostDevices, Picking, SettingsWindow};

fn edit_mode(name: &str, state: State) -> Mode {
    if state == State::Stopped {
        Mode::Edit(name.to_owned())
    } else {
        Mode::View(name.to_owned())
    }
}

impl Manager {
    pub(super) fn wire_settings(this: &Rc<RefCell<Self>>, settings: &SettingsWindow) {
        on!(this, settings, on_confirmed, |m| Self::discard(&m));
        let weak = Rc::downgrade(this);
        settings.window().on_close_requested(move || {
            let Some(m) = weak.upgrade() else {
                return CloseRequestResponse::HideWindow;
            };

            let mut mm = m.borrow_mut();

            if mm.editor.dirty {
                mm.ask_discard(Pending::Close);
                return CloseRequestResponse::KeepWindowShown;
            }

            mm.editor.hide();
            CloseRequestResponse::HideWindow
        });

        let edit = settings.global::<Edit>();
        let on_edit = this.borrow().on_edit.clone();
        edit.on_edited(move || on_edit());
        on!(this, edit, on_apply, |m| Self::apply(&m));
        on!(this, edit, on_revert, |m| {
            let mut m = m.borrow_mut();
            m.editor.revert();
            m.refresh_editor();
        });
        on!(this, edit, on_cancel_create, |m| {
            let mut m = m.borrow_mut();
            m.editor.hide();
            let _ = m.settings.hide();
        });
        on!(this, edit, on_browse_install, |m| Self::browse(
            &m,
            "Windows installation image",
            FileFilter::new("ISO images").glob("*.iso").glob("*.ISO"),
            None,
        ));
        on!(this, edit, on_browse_disk, |m, i| Self::browse(
            &m,
            "Disk image",
            FileFilter::new("Disk images")
                .glob("*.qcow2")
                .glob("*.img")
                .glob("*.raw"),
            usize::try_from(i).ok(),
        ));
        on!(this, edit, on_add_disk, |m, kind| m
            .borrow_mut()
            .editor
            .add_disk(kind, None));
        on!(this, edit, on_remove_disk, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.remove_disk(i);
            }
        });
        on!(this, edit, on_add_nic, |m| m.borrow().editor.add_nic());
        on!(this, edit, on_remove_nic, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.remove_nic(i);
            }
        });
        on!(this, edit, on_new_mac, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.new_mac(i);
            }
        });
        on!(this, edit, on_add_forward, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.add_forward(i);
            }
        });
        on!(this, edit, on_remove_forward, |m, i, j| {
            if let (Ok(i), Ok(j)) = (usize::try_from(i), usize::try_from(j)) {
                m.borrow().editor.remove_forward(i, j);
            }
        });
        on!(this, edit, on_remove_pci, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.remove_pci(i);
            }
        });
        on!(this, edit, on_remove_usb, |m, i| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.remove_usb(i);
            }
        });
        on!(this, edit, on_move_boot, |m, i, delta| {
            if let Ok(i) = usize::try_from(i) {
                m.borrow().editor.move_boot(i, delta);
            }
        });
        on!(this, edit, on_pick_device, |m, kind| Self::pick_device(
            &m, kind, None
        ));
        on!(this, edit, on_change_disk_device, |m, i| Self::pick_device(
            &m,
            Picking::Disks,
            usize::try_from(i).ok()
        ));

        let devices = settings.global::<HostDevices>();
        on!(this, devices, on_scan, |m| m.borrow().editor.scan_host());
        on!(this, devices, on_add_disk, |m, path| {
            let mut m = m.borrow_mut();
            let path = PathBuf::from(path.as_str());

            match m.editor.pick_disk.take() {
                Some(i) => m.editor.set_disk(i, DiskKind::Device, &path),
                None => m.editor.add_disk(DiskKind::Device, Some(path)),
            }
        });
        on!(this, devices, on_add_pci, |m, address| m
            .borrow_mut()
            .editor
            .add_pci(&address));
        on!(this, devices, on_add_usb, |m, id| m
            .borrow()
            .editor
            .add_usb(&id));
    }

    pub(super) fn open_settings(this: &Rc<RefCell<Self>>, target: Target) {
        let (shown, dirty) = {
            let m = this.borrow();

            let shown = match &m.editor.mode {
                Mode::Hidden => None,
                Mode::Create => Some(Target::New),
                Mode::Edit(n) | Mode::View(n) => Some(Target::Vm(n.clone())),
            };

            (shown == Some(target.clone()), m.editor.dirty)
        };

        if !shown && !dirty {
            Self::load_settings(this, target);
            return;
        }

        if !shown {
            this.borrow_mut().ask_discard(Pending::Open(target));
        }

        let settings = this.borrow().settings.clone_strong();
        present(settings.window());
    }

    fn load_settings(this: &Rc<RefCell<Self>>, target: Target) {
        let settings = {
            let mut m = this.borrow_mut();

            match target {
                Target::New => m.editor.create(),
                Target::Vm(name) => {
                    let Some((cfg, state)) = m.vm(&name).map(|v| (v.cfg.clone(), v.state)) else {
                        return;
                    };

                    m.editor.load(edit_mode(&name, state), &cfg);
                    m.set_log(&cfg, state);
                },
            }

            m.refresh_editor();
            m.refresh_model();
            m.settings.set_message(SharedString::new());
            m.settings.clone_strong()
        };
        present(settings.window());
    }

    fn ask_discard(&mut self, next: Pending) {
        let what = match &self.editor.mode {
            Mode::Create => "the new VM".to_owned(),
            mode => mode.name().unwrap_or_default().to_owned(),
        };

        self.settings
            .set_confirm_title(format!("Discard changes to {what}?").into());
        self.settings
            .set_confirm_text("They have not been applied.".into());
        self.settings.set_confirm_action("Discard".into());
        self.settings.set_confirm_open(true);
        self.pending = Some(next);
    }

    fn discard(this: &Rc<RefCell<Self>>) {
        let pending = {
            let mut m = this.borrow_mut();
            m.settings.set_confirm_open(false);
            m.editor.hide();
            m.pending.take()
        };

        match pending {
            Some(Pending::Open(target)) => Self::load_settings(this, target),
            Some(Pending::Close) => {
                let _ = this.borrow().settings.hide();
            },
            None => {},
        }
    }

    pub(super) fn sync_settings(&mut self) {
        let Some(name) = self.editor.mode.name().map(str::to_owned) else {
            return;
        };

        let Some((cfg, state)) = self.vm(&name).map(|v| (v.cfg.clone(), v.state)) else {
            self.editor.hide();
            let _ = self.settings.hide();
            return;
        };

        let mode = edit_mode(&name, state);

        if self.editor.mode != mode && !self.editor.dirty {
            self.editor.load(mode, &cfg);
        }

        self.set_log(&cfg, state);
        self.refresh_editor();
    }

    fn others(&self) -> Vec<String> {
        self.vms.iter().map(|v| v.cfg.name.clone()).collect()
    }

    pub(super) fn refresh_editor(&mut self) {
        let others = self.others();
        self.editor.refresh(&others);
    }

    fn set_log(&self, cfg: &VmConfig, state: State) {
        self.settings
            .global::<Edit>()
            .set_log(if state == State::Stopped {
                run::log_tail(&cfg.name, 15).into()
            } else {
                SharedString::new()
            });
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
                let state = m.vm(&cfg.name).map_or(State::Stopped, |v| v.state);
                m.editor.load(edit_mode(&cfg.name, state), &cfg);
                m.refresh_model();
                m.settings.set_message(SharedString::new());
                drop(m);
                Self::sync(this);
            },
            Err(e) => {
                m.settings.set_message(format!("{e:#}").into());
                m.refresh_editor();
            },
        }
    }

    fn browse(
        this: &Rc<RefCell<Self>>,
        title: &'static str,
        filter: FileFilter,
        disk: Option<usize>,
    ) {
        let (ui, generation) = {
            let m = this.borrow();
            (m.settings.as_weak(), m.editor.generation)
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
                    Some(i) => m.editor.set_disk(i, DiskKind::Image, &path),
                    None => m.editor.set_install(&path),
                },
                Ok(None) => {},
                Err(e) => ui.set_message(format!("{e:#}").into()),
            }
        });
    }

    fn pick_device(this: &Rc<RefCell<Self>>, kind: Picking, replacing: Option<usize>) {
        let mut m = this.borrow_mut();
        m.editor.scan_host();

        if kind == Picking::Disks {
            m.editor.pick_disk = replacing;
        }

        m.settings.global::<HostDevices>().set_picking(kind);
    }
}
