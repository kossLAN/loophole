use std::collections::BTreeMap;
use std::rc::Rc;

use loophole_vm::config::{Issue, Section, VmConfig};
use loophole_vm::{host, run};
use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, VecModel};

use crate::models::{self, Tracked};
use crate::ui::{
    BootEdit, DiskEdit, DiskKind, Edit, EditIssues, EditMode, NicEdit, PciEdit, SettingsWindow,
    UsbEdit,
};

mod devices;
mod draft;
mod fields;
mod rows;
mod show;

use self::fields::{
    TABS, create_tab, file_name, gib, gpu_window_sizes, hugepage_hints, hugepage_pools, index,
    options, tab, text,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Hidden,
    Create,
    Edit(String),
    View(String),
}

impl Mode {
    const fn ui(&self) -> EditMode {
        match self {
            Self::Hidden => EditMode::Hidden,
            Self::Create => EditMode::Create,
            Self::Edit(_) => EditMode::Edit,
            Self::View(_) => EditMode::View,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Edit(n) | Self::View(n) => Some(n),
            Self::Hidden | Self::Create => None,
        }
    }
}

const NEW_DISK_GIB: i32 = 128;

pub struct Draft {
    pub cfg: VmConfig,
    pub issues: Vec<Issue>,
    pub new_images: Vec<(usize, u32)>,
}

impl Draft {
    pub fn fatal(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.fatal)
    }
}

pub struct Editor {
    ui: slint::Weak<SettingsWindow>,
    pub mode: Mode,
    saved: Option<VmConfig>,
    gpu_window_sizes: Vec<u32>,
    disks: Rc<Tracked<DiskEdit>>,
    nics: Rc<Tracked<NicEdit>>,
    pci: Rc<Tracked<PciEdit>>,
    usb: Rc<VecModel<UsbEdit>>,
    boot: Rc<Tracked<BootEdit>>,
    warnings: Rc<VecModel<SharedString>>,
    tab_issues: Rc<VecModel<i32>>,
    on_edit: Rc<dyn Fn()>,
    next_id: i32,
    pub generation: u64,
    pub pick_disk: Option<usize>,
    pub dirty: bool,
}

impl Editor {
    pub fn new(ui: &SettingsWindow, on_edit: Rc<dyn Fn()>) -> Self {
        let nodes = host::render_nodes();
        let gpu_window_sizes = host::gpu(&nodes)
            .and_then(|gpu| gpu.vram)
            .map_or_else(Vec::new, gpu_window_sizes);
        let this = Self {
            ui: ui.as_weak(),
            mode: Mode::Hidden,
            saved: None,
            gpu_window_sizes,
            disks: Tracked::new(on_edit.clone()),
            nics: Tracked::new(on_edit.clone()),
            pci: Tracked::new(on_edit.clone()),
            usb: Rc::new(VecModel::default()),
            boot: Tracked::new(on_edit.clone()),
            warnings: Rc::new(VecModel::default()),
            tab_issues: Rc::new(VecModel::from(vec![0; TABS])),
            on_edit,
            next_id: 1,
            generation: 0,
            pick_disk: None,
            dirty: false,
        };
        let edit = ui.global::<Edit>();
        edit.set_disks(this.disks.model());
        edit.set_nics(this.nics.model());
        edit.set_pci(this.pci.model());
        edit.set_usb(ModelRc::from(this.usb.clone()));
        edit.set_boot(this.boot.model());
        edit.set_options(options(&this.gpu_window_sizes));
        edit.set_warnings(ModelRc::from(this.warnings.clone()));
        edit.set_tab_issues(ModelRc::from(this.tab_issues.clone()));
        edit.set_host_cpus(text(format!("Available: {} threads", host::cpu_count())));
        edit.set_host_memory(text(format!("Available: {}", gib(host::memory_total()))));
        let pools = hugepage_pools();
        edit.set_hugepages_available(pools.iter().any(Option::is_some));
        edit.set_host_hugepages(ModelRc::new(VecModel::from(hugepage_hints(&pools))));
        this
    }

    fn id(&mut self) -> i32 {
        self.next_id += 1;
        self.next_id
    }

    fn tab_of(&self, section: Section) -> usize {
        if self.mode == Mode::Create {
            create_tab(section)
        } else {
            tab(section)
        }
    }

    fn with_edit(&self, f: impl FnOnce(&Edit<'_>)) {
        if let Some(ui) = self.ui.upgrade() {
            f(&ui.global::<Edit>());
        }
    }

    pub fn hide(&mut self) {
        self.mode = Mode::Hidden;
        self.saved = None;
        self.dirty = false;
        self.generation += 1;
        self.with_edit(|e| {
            e.set_mode(EditMode::Hidden);
            e.set_dirty(false);
            e.set_summary(SharedString::new());
        });
    }

    pub fn load(&mut self, mode: Mode, cfg: &VmConfig) {
        let keep_tab = self.mode.name() == mode.name();
        self.mode = mode;
        self.saved = Some(cfg.clone());
        self.dirty = false;
        self.generation += 1;
        self.show(cfg);
        let mode = self.mode.ui();
        self.with_edit(|e| {
            e.set_mode(mode);

            if !keep_tab {
                e.set_tab(0);
            }
        });
    }

    pub fn create(&mut self) {
        self.mode = Mode::Create;
        self.saved = None;
        self.generation += 1;
        let mut cfg = VmConfig::new("");
        cfg.media.driver_disc = true;
        self.show(&cfg);
        self.disks.update(0, |d| d.kind = DiskKind::NewImage);
        self.refresh_disk_titles();
        self.with_edit(|e| {
            e.set_mode(EditMode::Create);
            e.set_tab(0);
        });
    }

    pub fn revert(&mut self) {
        if let Some(cfg) = self.saved.clone() {
            self.generation += 1;
            self.show(&cfg);
        }
    }

    pub fn refresh(&mut self, others: &[String]) {
        if self.mode == Mode::Hidden {
            return;
        }

        let Some(ui) = self.ui.upgrade() else { return };
        self.refresh_disk_titles();
        self.refresh_boot_labels();
        let draft = self.draft(others);
        let preflight: Vec<Issue> = run::preflight(&draft.cfg)
            .into_iter()
            .filter(|i| {
                !matches!(i.section, Section::Disk(d) if draft.new_images.iter().any(|(n, _)| *n == d))
            })
            .collect();
        self.dirty = match &self.mode {
            Mode::Create => true,
            _ => self.saved.as_ref() != Some(&draft.cfg) || !draft.new_images.is_empty(),
        };

        let mut by_section: BTreeMap<Section, Vec<String>> = BTreeMap::new();
        let mut warnings_by: BTreeMap<Section, Vec<String>> = BTreeMap::new();
        let mut tabs = vec![0; TABS + usize::from(self.mode == Mode::Create)];

        for i in &draft.issues {
            if i.fatal {
                by_section
                    .entry(i.section)
                    .or_default()
                    .push(i.message.clone());
                tabs[self.tab_of(i.section)] += 1;
            } else {
                warnings_by
                    .entry(i.section)
                    .or_default()
                    .push(i.message.clone());
            }
        }

        for i in &preflight {
            warnings_by
                .entry(i.section)
                .or_default()
                .push(i.message.clone());
        }

        let join = |m: &BTreeMap<Section, Vec<String>>, s: Section| {
            text(m.get(&s).map(|v| v.join("  ")).unwrap_or_default())
        };
        let edit = ui.global::<Edit>();
        edit.set_issues(EditIssues {
            general: join(&by_section, Section::General),
            cpu: join(&by_section, Section::Cpu),
            memory: join(&by_section, Section::Memory),
            gpu: join(&by_section, Section::Gpu),
            media: join(&by_section, Section::Media),
            sound: join(&by_section, Section::Sound),
            input: join(&by_section, Section::Input),
            boot: join(&by_section, Section::Boot),
            display: join(&by_section, Section::Display),
            machine: join(&by_section, Section::Machine),
        });

        for (i, d) in self.disks.to_vec().into_iter().enumerate() {
            let error = join(&by_section, Section::Disk(i));
            let warning = join(&warnings_by, Section::Disk(i));

            if d.error != error || d.warning != warning {
                self.disks.rows().set_row_data(
                    i,
                    DiskEdit {
                        error,
                        warning,
                        ..d
                    },
                );
            }
        }

        for (i, n) in self.nics.to_vec().into_iter().enumerate() {
            let error = join(&by_section, Section::Nic(i));

            if n.error != error {
                self.nics.rows().set_row_data(i, NicEdit { error, ..n });
            }
        }

        self.refresh_pci();
        let present = host::usb_devices();

        for (i, u) in self.usb.iter().enumerate() {
            let status = text(match present.iter().find(|d| u.id == d.id.to_string()) {
                None => "not plugged in",
                Some(d) if !d.accessible => "no access",
                Some(_) => "",
            });

            if u.status != status {
                self.usb.set_row_data(i, UsbEdit { status, ..u });
            }
        }

        let fatal: Vec<&Issue> = draft.fatal().collect();

        let summary = match fatal.as_slice() {
            [] => String::new(),
            [one] => one.message.clone(),
            [first, rest @ ..] => format!("{}  {} more", first.message, rest.len()),
        };

        let warnings: Vec<SharedString> = draft
            .issues
            .iter()
            .filter(|i| !i.fatal)
            .chain(preflight.iter())
            .map(|i| text(i.message.as_str()))
            .collect();
        edit.set_summary(text(summary));
        models::sync(&self.warnings, warnings);
        models::sync(&self.tab_issues, tabs);
        edit.set_dirty(self.dirty);
        edit.set_install_name(text(
            draft
                .cfg
                .media
                .install
                .as_deref()
                .map(file_name)
                .unwrap_or_default(),
        ));
    }

    pub fn show_problem(&self, draft: &Draft) {
        if let Some(first) = draft.fatal().next() {
            let t = self.tab_of(first.section);
            self.with_edit(|e| e.set_tab(index(t)));
        }
    }
}
