use std::time::Duration;

use loophole_display::keymap::linux_to_qnum;
use loophole_display::proxies::button;
use loophole_vm::Capture;
use slint::ComponentHandle as _;
use slint::winit_030::winit::dpi::PhysicalPosition;
use slint::winit_030::winit::event::{
    ElementState, MouseButton, MouseScrollDelta, Touch, TouchPhase, WindowEvent,
};
use slint::winit_030::winit::keyboard::{KeyCode, PhysicalKey};
use slint::winit_030::winit::platform::scancode::PhysicalKeyExtScancode as _;
use slint::winit_030::winit::window::CursorGrabMode;
use slint::winit_030::{EventResult, WinitWindowAccessor as _};

use super::buttons::{Owner, Route};
use super::{Screen, fit};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Input {
    Key(u32, bool),
    Abs(u32, u32),
    Rel(i32, i32),
    Button(u32, bool),
}

pub(super) fn coalesce(
    mut cur: Input,
    mut next: impl FnMut() -> Option<Input>,
) -> (Input, Option<Input>) {
    loop {
        match (cur, next()) {
            (c, None) => return (c, None),
            (Input::Abs(..), Some(Input::Abs(x, y))) => cur = Input::Abs(x, y),
            (Input::Rel(a, b), Some(Input::Rel(x, y))) => {
                cur = Input::Rel(a.saturating_add(x), b.saturating_add(y));
            },
            (c, Some(other)) => return (c, Some(other)),
        }
    }
}

fn guest_position(
    local: (f64, f64),
    size: (f64, f64),
    frame: (u32, u32),
    clamp: bool,
) -> Option<(u32, u32)> {
    let (fw, fh) = (f64::from(frame.0), f64::from(frame.1));

    if fw < 1.0 || fh < 1.0 {
        return None;
    }

    let (rx, ry, rw, rh) = fit(size.0, size.1, fw, fh);
    let (fx, fy) = ((local.0 - rx) / rw, (local.1 - ry) / rh);
    let inside = (0.0..1.0).contains(&fx) && (0.0..1.0).contains(&fy);

    if !inside && !clamp {
        return None;
    }

    let axis = |f: f64, n: f64| (f * n).clamp(0.0, n - 1.0) as u32;
    Some((axis(fx, fw), axis(fy, fh)))
}

const PIXELS_PER_NOTCH: f64 = 10.0;
const MAX_NOTCHES: i32 = 20;

fn wheel_notches(acc: &mut f64, delta: f64) -> i32 {
    *acc += delta;
    let whole = acc.trunc();
    *acc -= whole;
    whole.clamp(-f64::from(MAX_NOTCHES), f64::from(MAX_NOTCHES)) as i32
}

impl Screen {
    fn over_screen(&self) -> bool {
        self.pointer.is_some()
    }

    pub(super) fn update_pointer(&mut self) {
        let (x, y, w, h) = self.rect;
        self.pointer = self
            .host
            .map(|(hx, hy)| (hx - x, hy - y))
            .filter(|&(px, py)| (0.0..w).contains(&px) && (0.0..h).contains(&py));
    }

    fn send_position(&mut self, clamp: bool) {
        if self.captured {
            return;
        }

        let Some(frame) = self.frame_size() else {
            return;
        };

        let Some((hx, hy)) = self.host else { return };
        let local = (hx - self.rect.0, hy - self.rect.1);

        let Some(pos) = guest_position(local, (self.rect.2, self.rect.3), frame, clamp) else {
            return;
        };

        if self.last_abs == Some(pos) {
            return;
        }

        self.last_abs = Some(pos);
        self.send(Input::Abs(pos.0, pos.1));
        self.probe();
    }

    fn probe(&self) {
        if self.opts.capture != Capture::Auto
            || self.cursor_visible
            || self.hidden_confirmed
            || self.probe_timer.running()
        {
            return;
        }

        let weak = self.me.clone();
        self.probe_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(150),
            move || {
                if let Some(s) = weak.upgrade() {
                    let mut s = s.borrow_mut();
                    s.hidden_confirmed = !s.cursor_visible;
                    s.update_capture();
                }
            },
        );
    }

    fn cursor_moved(&mut self, position: PhysicalPosition<f64>) {
        let Some(ui) = self.ui.upgrade() else { return };
        let scale = f64::from(ui.window().scale_factor());
        self.host = Some((position.x / scale, position.y / scale));

        if self.captured {
            return;
        }

        self.update_pointer();
        let dragging = self.buttons.owner() == Owner::Guest;

        if dragging || (self.takes_pointer() && self.over_screen()) {
            self.send_position(dragging);
        }

        self.update_cursor();
        self.update_capture();
    }

    fn cursor_left(&mut self) {
        if self.captured || self.buttons.owner() == Owner::Guest {
            return;
        }

        self.host = None;
        self.pointer = None;
        self.update_cursor();
        self.update_capture();
    }

    fn to_guest(&self) -> bool {
        self.captured || (self.takes_pointer() && self.over_screen())
    }

    fn mouse_input(&mut self, pressed: bool, b: MouseButton) -> EventResult {
        let to_guest = self.to_guest();

        if pressed && to_guest && self.buttons.idle() {
            self.send_position(true);
        }

        match self.buttons.input(pressed, b, to_guest) {
            Route::Host => EventResult::Propagate,
            Route::Guest(change) => {
                if let Some((q, p)) = change {
                    self.send(Input::Button(q, p));
                }

                if !self.buttons.guest_down() {
                    self.update_pointer();
                    self.update_capture();
                }

                EventResult::PreventDefault
            },
        }
    }

    fn mouse_wheel(&mut self, delta: MouseScrollDelta, phase: TouchPhase) -> EventResult {
        let to_guest = match self.buttons.owner() {
            Owner::Guest => true,
            Owner::Host => false,
            Owner::None => self.to_guest(),
        };

        if !to_guest {
            return EventResult::Propagate;
        }

        if phase == TouchPhase::Started {
            self.wheel = 0.0;
        }

        let notches = match delta {
            MouseScrollDelta::LineDelta(_, y) => f64::from(y),
            MouseScrollDelta::PixelDelta(p) => {
                let scale = self
                    .ui
                    .upgrade()
                    .map_or(1.0, |ui| f64::from(ui.window().scale_factor()));

                p.y / scale / PIXELS_PER_NOTCH
            },
        };

        let n = wheel_notches(&mut self.wheel, notches);

        let b = if n > 0 {
            button::WHEEL_UP
        } else {
            button::WHEEL_DOWN
        };

        for _ in 0..n.unsigned_abs() {
            self.send(Input::Button(b, true));
            self.send(Input::Button(b, false));
        }

        EventResult::PreventDefault
    }

    fn touch(&mut self, t: &Touch) -> EventResult {
        match t.phase {
            TouchPhase::Started if self.touch.is_none() => {
                self.cursor_moved(t.location);

                if !self.to_guest() {
                    return EventResult::Propagate;
                }

                self.touch = Some(t.id);
                self.mouse_input(true, MouseButton::Left)
            },
            TouchPhase::Moved if self.touch == Some(t.id) => {
                self.cursor_moved(t.location);
                EventResult::PreventDefault
            },
            TouchPhase::Ended | TouchPhase::Cancelled if self.touch == Some(t.id) => {
                self.touch = None;
                self.mouse_input(false, MouseButton::Left)
            },
            _ => EventResult::Propagate,
        }
    }

    pub(super) fn update_capture(&mut self) {
        // Switching QEMU's pointing device replays held buttons as an extra click.
        if self.buttons.guest_down() {
            return;
        }

        let wanted = match self.opts.capture {
            Capture::Auto => {
                self.keyboard_to_guest && !self.cursor_visible && self.hidden_confirmed
            },
            Capture::Manual => self.capture_requested,
            Capture::Never => false,
        };

        let on =
            wanted && self.focused && self.takes_pointer() && (self.captured || self.over_screen());
        self.set_captured(on);
    }

    pub(super) fn set_captured(&mut self, on: bool) {
        if on == self.captured {
            return;
        }

        let Some(ui) = self.ui.upgrade() else { return };
        let grabbed = ui.window().with_winit_window(|w| {
            if on {
                w.set_cursor_grab(CursorGrabMode::Locked)
                    .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined))
                    .map_err(|e| eprintln!("loophole: cannot hold the pointer: {e}"))
                    .is_ok()
            } else {
                let _ = w.set_cursor_grab(CursorGrabMode::None);
                true
            }
        });

        if on && grabbed != Some(true) {
            return;
        }

        self.captured = on;
        self.motion = (0.0, 0.0);
        self.last_abs = None;

        if on {
            self.send(Input::Rel(0, 0));
        } else {
            self.update_pointer();
            self.send_position(false);
        }

        self.update_cursor();
        self.update_hint();
    }

    pub(super) fn raw_motion(&mut self, dx: f64, dy: f64) {
        if !self.captured {
            return;
        }

        self.motion.0 += dx;
        self.motion.1 += dy;
        let (x, y) = (self.motion.0.trunc(), self.motion.1.trunc());

        if x != 0.0 || y != 0.0 {
            self.motion.0 -= x;
            self.motion.1 -= y;
            self.send(Input::Rel(x as i32, y as i32));
        }
    }

    fn takes_pointer(&self) -> bool {
        self.display.is_some()
    }

    fn takes_keyboard(&self) -> bool {
        self.keyboard_to_guest && self.takes_pointer()
    }

    pub(super) fn release_all(&mut self) {
        for q in std::mem::take(&mut self.keys_down) {
            self.send(Input::Key(q, false));
        }

        for b in self.buttons.release_all() {
            self.send(Input::Button(b, false));
        }

        self.touch = None;
    }

    pub(super) fn winit_event(&mut self, event: &WindowEvent) -> EventResult {
        match event {
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                EventResult::Propagate
            },
            WindowEvent::Focused(on) => {
                self.focused = *on;

                if !on {
                    self.release_all();
                    self.capture_requested = false;
                }

                self.update_capture();
                EventResult::Propagate
            },
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_moved(*position);
                EventResult::Propagate
            },
            WindowEvent::CursorLeft { .. } => {
                self.cursor_left();
                EventResult::Propagate
            },
            WindowEvent::MouseInput { state, button, .. } => {
                self.mouse_input(*state == ElementState::Pressed, *button)
            },
            WindowEvent::MouseWheel { delta, phase, .. } => self.mouse_wheel(*delta, *phase),
            WindowEvent::Touch(t) => self.touch(t),
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;

                if pressed
                    && self.modifiers.control_key()
                    && self.modifiers.alt_key()
                    && self.shortcut(event.physical_key)
                {
                    return EventResult::PreventDefault;
                }

                if !self.takes_keyboard() {
                    return EventResult::Propagate;
                }

                let qnum = event.physical_key.to_scancode().and_then(linux_to_qnum);

                if let Some(q) = qnum {
                    if pressed {
                        self.keys_down.insert(q);
                    } else {
                        self.keys_down.remove(&q);
                    }

                    self.send(Input::Key(q, pressed));
                }

                EventResult::PreventDefault
            },
            _ => EventResult::Propagate,
        }
    }

    fn shortcut(&mut self, key: PhysicalKey) -> bool {
        let Some(ui) = self.ui.upgrade() else {
            return false;
        };

        let PhysicalKey::Code(code) = key else {
            return false;
        };

        match code {
            KeyCode::KeyF => {
                ui.invoke_toggle_fullscreen();
            },
            KeyCode::KeyG => {
                self.release_all();

                if self.opts.capture == Capture::Manual {
                    self.capture_requested = !self.capture_requested;
                    self.keyboard_to_guest = true;
                } else {
                    self.keyboard_to_guest = !self.keyboard_to_guest;
                }

                self.update_capture();
                self.update_hint();
            },
            KeyCode::KeyS => ui.set_stats_visible(!ui.get_stats_visible()),
            _ => return false,
        }

        true
    }
}
