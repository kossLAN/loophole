use std::collections::BTreeSet;

use loophole_display::proxies::button;
use slint::winit_030::winit::event::MouseButton;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Owner {
    #[default]
    None,
    Guest,
    Host,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Route {
    Host,
    Guest(Option<(u32, bool)>),
}

const fn qemu_button(b: MouseButton) -> Option<u32> {
    match b {
        MouseButton::Left => Some(button::LEFT),
        MouseButton::Middle => Some(button::MIDDLE),
        MouseButton::Right => Some(button::RIGHT),
        MouseButton::Back => Some(button::SIDE),
        MouseButton::Forward => Some(button::EXTRA),
        MouseButton::Other(_) => None,
    }
}

#[derive(Default)]
pub(super) struct Buttons {
    held: BTreeSet<MouseButton>,
    owner: Owner,
    down: BTreeSet<u32>,
}

impl Buttons {
    pub(super) fn input(&mut self, pressed: bool, b: MouseButton, to_guest: bool) -> Route {
        if pressed {
            if self.held.is_empty() {
                self.owner = if to_guest { Owner::Guest } else { Owner::Host };
            }

            self.held.insert(b);
        } else if !self.held.remove(&b) {
            return Route::Host;
        }

        let owner = self.owner;

        if self.held.is_empty() {
            self.owner = Owner::None;
        }

        if owner != Owner::Guest {
            return Route::Host;
        }

        let changed = qemu_button(b).filter(|&q| {
            if pressed {
                self.down.insert(q)
            } else {
                self.down.remove(&q)
            }
        });

        Route::Guest(changed.map(|q| (q, pressed)))
    }

    pub(super) fn release_all(&mut self) -> BTreeSet<u32> {
        self.held.clear();
        self.owner = Owner::None;
        std::mem::take(&mut self.down)
    }

    pub(super) fn guest_down(&self) -> bool {
        !self.down.is_empty()
    }

    pub(super) const fn owner(&self) -> Owner {
        self.owner
    }

    pub(super) fn idle(&self) -> bool {
        self.held.is_empty()
    }
}
