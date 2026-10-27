use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle as _;

use super::Manager;
use crate::disc::{self, Status};
use crate::ui::Edit;

impl Manager {
    pub(super) fn driver_disc_available(&self) -> bool {
        self.tools.as_ref().is_ok_and(|t| t.driver_disc.is_some())
    }

    pub(super) fn show_disc(&self) {
        let available = self.driver_disc_available();
        let edit = self.settings.global::<Edit>();
        edit.set_driver_disc_available(available);
        edit.set_driver_disc_name(self.disc.name().into());
        edit.set_driver_disc_hint(self.disc.hint(available).into());
    }

    pub(super) fn fetch_disc(this: &Rc<RefCell<Self>>) {
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
}
