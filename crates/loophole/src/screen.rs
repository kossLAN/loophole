#![expect(
    clippy::print_stderr,
    reason = "what the window cannot show goes to stderr"
)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use loophole_display::dmabuf::BufferKey;
use loophole_display::keymap::LINUX_TO_QNUM;
use loophole_display::listener::{CursorShape, Event};
use loophole_display::proxies::button;
use loophole_display::{Display, Source};
use loophole_vm::{Capture, qmp};
use slint::winit_030::winit::dpi::PhysicalPosition;
use slint::winit_030::winit::event::{
    ElementState, MouseButton, MouseScrollDelta, Touch, TouchPhase, WindowEvent,
};
use slint::winit_030::winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use slint::winit_030::winit::platform::scancode::PhysicalKeyExtScancode as _;
use slint::winit_030::winit::window::CursorGrabMode;
use slint::winit_030::{EventResult, WinitWindowAccessor as _};
use slint::{
    BorrowedOpenGLTextureBuilder, BorrowedOpenGLTextureOrigin, ComponentHandle as _, GraphicsAPI,
    Image, RenderingState, Rgba8Pixel, SharedPixelBuffer,
};

use crate::MainWindow;
use crate::gl::Gl;
use crate::unblock;

#[derive(Clone, Default)]
pub struct Options {
    pub resolution: Option<(u32, u32)>,
    pub refresh: Option<f64>,
    pub log_stats: bool,
    pub capture: Capture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    Key(u32, bool),
    Abs(u32, u32),
    Rel(i32, i32),
    Button(u32, bool),
}

// Merges motion that queued up behind a slow call, never across a key or button.
fn coalesce(mut cur: Input, mut next: impl FnMut() -> Option<Input>) -> (Input, Option<Input>) {
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Owner {
    #[default]
    None,
    Guest,
    Host,
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
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

// Whoever gets the first press keeps every button until all are up, as an implicit grab.
#[derive(Default)]
struct Buttons {
    held: BTreeSet<MouseButton>,
    owner: Owner,
    down: BTreeSet<u32>,
}

impl Buttons {
    fn input(&mut self, pressed: bool, b: MouseButton, to_guest: bool) -> Route {
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

    fn release_all(&mut self) -> BTreeSet<u32> {
        self.held.clear();
        self.owner = Owner::None;
        std::mem::take(&mut self.down)
    }

    fn guest_down(&self) -> bool {
        !self.down.is_empty()
    }
}

// The largest rectangle with the frame's aspect ratio centred in (w, h): x, y, width, height.
fn fit(w: f64, h: f64, fw: f64, fh: f64) -> (f64, f64, f64, f64) {
    let s = (w / fw).min(h / fh);
    let (dw, dh) = (fw * s, fh * s);
    ((w - dw) / 2.0, (h - dh) / 2.0, dw, dh)
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
    // QEMU refuses x >= width.
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

fn bgrx_image(width: u32, height: u32, stride: u32, data: &[u8], alpha: bool) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    let out = buf.make_mut_bytes();
    for y in 0..height as usize {
        let row = &data[y * stride as usize..][..width as usize * 4];
        let (pixels, _) = row.as_chunks::<4>();
        for (x, &[b, g, r, a]) in pixels.iter().enumerate() {
            let o = (y * width as usize + x) * 4;
            out[o..o + 4].copy_from_slice(&[r, g, b, if alpha { a } else { 255 }]);
        }
    }
    Image::from_rgba8(buf)
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent parts of where input goes"
)]
pub struct Screen {
    me: Weak<RefCell<Self>>,
    ui: slint::Weak<MainWindow>,
    opts: Options,
    display: Option<Display>,
    tasks: Vec<slint::JoinHandle<()>>,
    input: Option<async_channel::Sender<Input>>,
    on_gone: Option<Rc<dyn Fn()>>,

    gl: Option<Gl>,
    gl_error: Option<String>,
    shown_key: Option<(BufferKey, bool)>,
    shown_memory: Option<u64>,
    import_error: Option<String>,

    cursor: Option<(i32, i32, i32, i32)>,
    cursor_visible: bool,
    guest_cursor: Option<(i32, i32)>,
    // The Screen element and the host pointer, in window logical px.
    rect: (f64, f64, f64, f64),
    host: Option<(f64, f64)>,
    // The host pointer relative to the Screen element, while it is over it.
    pointer: Option<(f64, f64)>,
    buttons: Buttons,
    touch: Option<u64>,
    last_abs: Option<(u32, u32)>,
    wheel: f64,

    keyboard_to_guest: bool,
    focused: bool,
    captured: bool,
    capture_requested: bool,
    hidden_confirmed: bool,
    probe_timer: slint::Timer,
    // Raw motion not sent yet: the guest takes whole counts.
    motion: (f64, f64),
    keys_down: BTreeSet<u32>,
    modifiers: ModifiersState,

    resize_timer: slint::Timer,
    check_timer: slint::Timer,
    stats_timer: slint::Timer,
    sent_size: Option<(u32, u32)>,
    sent_refresh: Option<u32>,

    presented: u32,
    stats_at: Instant,
}

type MotionHandler = Box<dyn FnMut(f64, f64)>;

thread_local! {
    static RAW_MOTION: RefCell<Option<MotionHandler>> = RefCell::new(None);
}

pub fn raw_motion(dx: f64, dy: f64) {
    RAW_MOTION.with(|m| {
        if let Some(f) = m.borrow_mut().as_mut() {
            f(dx, dy);
        }
    });
}

impl Screen {
    pub fn install(ui: &MainWindow, opts: Options) -> Rc<RefCell<Self>> {
        let this = Rc::new_cyclic(|me| {
            RefCell::new(Self {
                me: me.clone(),
                ui: ui.as_weak(),
                opts,
                display: None,
                tasks: Vec::new(),
                input: None,
                on_gone: None,
                gl: None,
                gl_error: None,
                shown_key: None,
                shown_memory: None,
                import_error: None,
                cursor: None,
                cursor_visible: true,
                guest_cursor: None,
                rect: (0.0, 0.0, 0.0, 0.0),
                host: None,
                pointer: None,
                buttons: Buttons::default(),
                touch: None,
                last_abs: None,
                wheel: 0.0,
                keyboard_to_guest: true,
                focused: true,
                captured: false,
                capture_requested: false,
                hidden_confirmed: false,
                probe_timer: slint::Timer::default(),
                motion: (0.0, 0.0),
                keys_down: BTreeSet::new(),
                modifiers: ModifiersState::empty(),
                resize_timer: slint::Timer::default(),
                check_timer: slint::Timer::default(),
                stats_timer: slint::Timer::default(),
                sent_size: None,
                sent_refresh: None,
                presented: 0,
                stats_at: Instant::now(),
            })
        });

        let s = this.clone();
        let notifier = ui.window().set_rendering_notifier(move |state, api| {
            let mut s = s.borrow_mut();
            match state {
                RenderingState::RenderingSetup => {
                    let gl = match api {
                        GraphicsAPI::NativeOpenGL { get_proc_address } => Gl::new(get_proc_address),
                        _ => Err(anyhow::anyhow!("the window does not render with OpenGL")),
                    };
                    match gl {
                        Ok(gl) => s.gl = Some(gl),
                        Err(e) => {
                            eprintln!("loophole: no zero-copy display: {e:#}");
                            s.gl_error = Some(format!("{e:#}"));
                        },
                    }
                },
                RenderingState::BeforeRendering => s.before_render(),
                RenderingState::AfterRendering => s.presented += 1,
                RenderingState::RenderingTeardown => {
                    // Textures go with the context, which is current now.
                    s.gl = None;
                    s.shown_key = None;
                },
                _ => {},
            }
        });
        if let Err(e) = notifier {
            this.borrow_mut().gl_error = Some(format!("no OpenGL renderer: {e}"));
        }

        let s = this.clone();
        ui.window()
            .on_winit_window_event(move |_, event| s.borrow_mut().winit_event(event));
        let weak = Rc::downgrade(&this);
        RAW_MOTION.with(|m| {
            *m.borrow_mut() = Some(Box::new(move |dx, dy| {
                if let Some(s) = weak.upgrade() {
                    s.borrow_mut().raw_motion(dx, dy);
                }
            }));
        });

        let weak = Rc::downgrade(&this);
        ui.on_screen_geometry(move |x, y, w, h| {
            if let Some(s) = weak.upgrade() {
                s.borrow_mut().geometry(x, y, w, h);
            }
        });

        let weak = Rc::downgrade(&this);
        this.borrow().check_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(2),
            move || {
                if let Some(s) = weak.upgrade() {
                    Self::check(&s);
                }
            },
        );
        let weak = Rc::downgrade(&this);
        this.borrow().stats_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if let Some(s) = weak.upgrade() {
                    s.borrow_mut().update_stats();
                }
            },
        );

        this
    }

    pub fn set_on_gone(&mut self, f: impl Fn() + 'static) {
        self.on_gone = Some(Rc::new(f));
    }

    pub fn set_options(&mut self, opts: Options) {
        self.opts = opts;
        self.capture_requested = false;
        self.hidden_confirmed = false;
        self.sent_size = None;
        self.sent_refresh = None;
        self.update_capture();
        self.update_hint();
    }

    fn set_placeholder(&self, text: &str) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_placeholder(text.into());
        }
    }

    pub fn attach(this: &Rc<RefCell<Self>>, qmp: PathBuf) {
        Self::detach(this);
        this.borrow().set_placeholder("Connecting…");
        let weak = Rc::downgrade(this);
        let task = slint::spawn_local(async move {
            let result = match unblock(move || qmp::attach_display(&qmp)).await {
                Ok(stream) => Display::connect(stream).await,
                Err(e) => Err(e),
            };
            let Some(this) = weak.upgrade() else { return };
            let (display, rx) = match result {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("loophole: {e:#}");
                    this.borrow()
                        .set_placeholder(&format!("Cannot show the VM: {e:#}"));
                    return;
                },
            };
            let (itx, irx) = async_channel::unbounded::<Input>();
            let keyboard = display.keyboard.clone();
            let mouse = display.mouse.clone();
            {
                let mut s = this.borrow_mut();
                if let Some(ui) = s.ui.upgrade() {
                    if !display.name.is_empty() {
                        ui.set_window_title(format!("{} – loophole", display.name).into());
                    }
                    ui.set_placeholder("".into());
                }
                s.display = Some(display);
                s.input = Some(itx);
                s.sent_size = None;
                s.sent_refresh = None;
                s.shown_key = None;
                s.shown_memory = None;
                s.last_abs = None;
            }
            Self::send_size(&this);
            Self::check(&this);

            // Ends once detach drops the sender and the releases it queued have gone out.
            let _input = slint::spawn_local(async move {
                let mut next = None;
                let mut rel_failed = false;
                loop {
                    let first = match next.take() {
                        Some(i) => i,
                        None => match irx.recv().await {
                            Ok(i) => i,
                            Err(_) => break,
                        },
                    };
                    let (i, rest) = coalesce(first, || irx.try_recv().ok());
                    next = rest;
                    let r = match i {
                        Input::Key(q, true) => keyboard.press(q).await,
                        Input::Key(q, false) => keyboard.release(q).await,
                        Input::Abs(x, y) => mouse.set_abs_position(x, y).await,
                        Input::Rel(dx, dy) => {
                            let r = mouse.rel_motion(dx, dy).await;
                            if r.is_err() && std::mem::replace(&mut rel_failed, true) {
                                continue;
                            }
                            r
                        },
                        Input::Button(b, true) => mouse.press(b).await,
                        Input::Button(b, false) => mouse.release(b).await,
                    };
                    if let Err(e) = r {
                        eprintln!("loophole: input: {e}");
                    }
                }
            });

            let weak = Rc::downgrade(&this);
            drop(this);
            while let Ok(e) = rx.recv().await {
                let Some(this) = weak.upgrade() else { return };
                this.borrow_mut().event(e);
            }
            if let Some(this) = weak.upgrade() {
                Self::gone(&this);
            }
        });
        if let Ok(t) = task {
            this.borrow_mut().tasks.push(t);
        }
    }

    pub fn detach(this: &Rc<RefCell<Self>>) {
        let mut s = this.borrow_mut();
        s.release_all();
        s.set_captured(false);
        for t in s.tasks.drain(..) {
            t.abort();
        }
        s.display = None;
        s.input = None;
        s.cursor = None;
        s.shown_memory = None;
        s.probe_timer.stop();
        if let Some(ui) = s.ui.upgrade() {
            ui.set_frame(Image::default());
            ui.set_cursor_visible(false);
            ui.set_window_title("loophole".into());
            ui.window().request_redraw();
        }
    }

    fn gone(this: &Rc<RefCell<Self>>) {
        if this.borrow().display.is_none() {
            return;
        }
        eprintln!("loophole: the VM went away");
        Self::detach(this);
        let f = this.borrow().on_gone.clone();
        if let Some(f) = f {
            f();
        }
    }

    fn send(&self, i: Input) {
        if let Some(tx) = &self.input {
            let _ = tx.try_send(i);
        }
    }

    fn event(&mut self, e: Event) {
        let Some(d) = self.display.as_mut() else {
            return;
        };
        let changed = d.event(e);
        if let Some(why) = changed.rejected {
            eprintln!("loophole: {why}");
        }
        if let Some(shape) = changed.cursor {
            self.set_cursor(shape);
        }
        if let Some(at) = changed.cursor_at {
            self.guest_cursor = Some(at);
            if self.captured {
                self.update_cursor();
            }
        }
        // QEMU repeats MouseSet with every guest move.
        if let Some(on) = changed.cursor_visible
            && on != self.cursor_visible
        {
            self.cursor_visible = on;
            self.hidden_confirmed = false;
            self.probe_timer.stop();
            self.update_cursor();
            self.update_capture();
        }
        if changed.frame {
            self.frame_changed();
        }
    }

    fn frame_changed(&mut self) {
        let (Some(ui), Some(d)) = (self.ui.upgrade(), self.display.as_ref()) else {
            return;
        };
        if let Some(Source::Memory {
            width,
            height,
            stride,
            data,
        }) = &d.source
            && self.shown_memory != Some(d.serial)
        {
            ui.set_frame(bgrx_image(*width, *height, *stride, data, false));
            self.shown_memory = Some(d.serial);
            self.shown_key = None;
        }
        ui.window().request_redraw();
    }

    fn before_render(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(gl) = self.gl.as_mut() else {
            if let Some(e) = &self.gl_error {
                ui.set_placeholder(e.as_str().into());
            }
            return;
        };
        let Some(display) = self.display.as_ref() else {
            gl.clear();
            self.shown_key = None;
            return;
        };
        let mut keep = None;
        if let Some(Source::Dmabuf {
            buf,
            key: Some(key),
        }) = &display.source
        {
            keep = Some(*key);
            if self.shown_key != Some((*key, display.y0_top)) {
                match gl.texture(*key, buf) {
                    Ok(t) => {
                        // SAFETY: the texture lives until evicted, and it is not evicted while
                        // shown.
                        let image = unsafe {
                            BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                                t.id,
                                (t.width, t.height).into(),
                            )
                        }
                        .origin(if display.y0_top {
                            BorrowedOpenGLTextureOrigin::BottomLeft
                        } else {
                            BorrowedOpenGLTextureOrigin::TopLeft
                        })
                        .build();
                        ui.set_frame(image);
                        self.shown_key = Some((*key, display.y0_top));
                        self.shown_memory = None;
                        self.import_error = None;
                    },
                    Err(e) => {
                        let msg = format!("{}: {e:#}", display.label);
                        if self.import_error.as_deref() != Some(msg.as_str()) {
                            eprintln!("loophole: {msg}");
                            self.import_error = Some(msg);
                        }
                    },
                }
            }
        }
        // The texture on screen stays even if the new one failed to import.
        let shown = self.shown_key.map(|(k, _)| k);
        gl.evict(
            &[keep, shown].into_iter().flatten().collect::<Vec<_>>(),
            Duration::from_secs(2),
        );
    }

    fn set_cursor(&mut self, shape: CursorShape) {
        let Some(ui) = self.ui.upgrade() else { return };
        if shape.width <= 0
            || shape.height <= 0
            || shape.data.len() < (shape.width * shape.height * 4) as usize
        {
            return;
        }
        let (w, h) = (shape.width as u32, shape.height as u32);
        ui.set_cursor(bgrx_image(w, h, w * 4, &shape.data, true));
        self.cursor = Some((shape.width, shape.height, shape.hot_x, shape.hot_y));
        self.update_cursor();
    }

    fn frame_size(&self) -> Option<(u32, u32)> {
        Some(self.display.as_ref()?.source.as_ref()?.size())
    }

    // Uncaptured, the guest's pointer is where the host's is (absolute device), a frame later.
    // Captured, it is wherever relative motion took it.
    fn update_cursor(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let geometry = (|| {
            let (fw, fh) = self.frame_size()?;
            let (cw, ch, hx, hy) = self.cursor?;
            let (rx, ry, rw, _) = fit(self.rect.2, self.rect.3, f64::from(fw), f64::from(fh));
            let s = rw / f64::from(fw);
            let (px, py) = if self.captured {
                let (gx, gy) = self.guest_cursor?;
                (f64::from(gx).mul_add(s, rx), f64::from(gy).mul_add(s, ry))
            } else {
                self.pointer?
            };
            Some((
                f64::from(hx).mul_add(-s, px),
                f64::from(hy).mul_add(-s, py),
                f64::from(cw) * s,
                f64::from(ch) * s,
            ))
        })();
        match geometry {
            Some((x, y, w, h)) if self.cursor_visible => {
                ui.set_cursor_x(x as f32);
                ui.set_cursor_y(y as f32);
                ui.set_cursor_w(w as f32);
                ui.set_cursor_h(h as f32);
                ui.set_cursor_visible(true);
            },
            _ => ui.set_cursor_visible(false),
        }
    }

    fn geometry(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let rect = (f64::from(x), f64::from(y), f64::from(w), f64::from(h));
        let resized = (rect.2, rect.3) != (self.rect.2, self.rect.3);
        self.rect = rect;
        self.update_pointer();
        self.update_cursor();
        if !resized {
            return;
        }
        let weak = self.me.clone();
        self.resize_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(400),
            move || {
                if let Some(s) = weak.upgrade() {
                    Self::send_size(&s);
                }
            },
        );
    }

    fn over_screen(&self) -> bool {
        self.pointer.is_some()
    }

    fn update_pointer(&mut self) {
        let (x, y, w, h) = self.rect;
        self.pointer = self
            .host
            .map(|(hx, hy)| (hx - x, hy - y))
            .filter(|&(px, py)| (0.0..w).contains(&px) && (0.0..h).contains(&py));
    }

    fn send_position(&mut self, clamp: bool) {
        // A locked pointer may still report positions (X11 only confines it). A position makes
        // the tablet the active device again.
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

    // Windows and video players show a hidden pointer again on the next motion. Games keep it
    // hidden.
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
        let dragging = self.buttons.owner == Owner::Guest;
        if dragging || (self.takes_pointer() && self.over_screen()) {
            self.send_position(dragging);
        }
        self.update_cursor();
        self.update_capture();
    }

    fn cursor_left(&mut self) {
        if self.captured || self.buttons.owner == Owner::Guest {
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
        if pressed && to_guest && self.buttons.held.is_empty() {
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
        let to_guest = match self.buttons.owner {
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

    fn update_capture(&mut self) {
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

    fn set_captured(&mut self, on: bool) {
        if on == self.captured {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let grabbed = ui.window().with_winit_window(|w| {
            if on {
                // Wayland locks the pointer. X11 can only confine it.
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
        // Switches QEMU's pointing device.
        if on {
            self.send(Input::Rel(0, 0));
        } else {
            self.update_pointer();
            self.send_position(false);
        }
        self.update_cursor();
        self.update_hint();
    }

    fn raw_motion(&mut self, dx: f64, dy: f64) {
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
        let Some(ui) = self.ui.upgrade() else {
            return false;
        };
        self.display.is_some()
            && (ui.get_show_screen() || !ui.get_manager())
            && !ui.get_modal_open()
    }

    fn takes_keyboard(&self) -> bool {
        self.keyboard_to_guest && self.takes_pointer()
    }

    fn release_all(&mut self) {
        for q in std::mem::take(&mut self.keys_down) {
            self.send(Input::Key(q, false));
        }
        for b in self.buttons.release_all() {
            self.send(Input::Button(b, false));
        }
        self.touch = None;
    }

    fn winit_event(&mut self, event: &WindowEvent) -> EventResult {
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
                let qnum = event
                    .physical_key
                    .to_scancode()
                    .and_then(|code| LINUX_TO_QNUM.get(code as usize).copied())
                    .filter(|&q| q != 0);
                if let Some(q) = qnum.map(u32::from) {
                    // Repeats are presses again, as a keyboard's typematic.
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

    pub fn update_hint(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let hint = if self.display.is_none() {
            ""
        } else if self.captured {
            "Keyboard and pointer → guest · Ctrl+Alt+G takes them back"
        } else if !self.keyboard_to_guest {
            "Keyboard → loophole · Ctrl+Alt+G gives it to the guest"
        } else if self.opts.capture == Capture::Manual {
            "Keyboard → guest · Ctrl+Alt+G captures the pointer · Ctrl+Alt+F fullscreen · Ctrl+Alt+S stats"
        } else {
            "Keyboard → guest · Ctrl+Alt+G takes it back · Ctrl+Alt+F fullscreen · Ctrl+Alt+S stats"
        };
        ui.set_hint(hint.into());
    }

    // In device pixels.
    fn send_size(this: &Rc<RefCell<Self>>) {
        let (console, size) = {
            let s = this.borrow();
            let (Some(ui), Some(d)) = (s.ui.upgrade(), s.display.as_ref()) else {
                return;
            };
            let size = s.opts.resolution.unwrap_or_else(|| {
                let scale = f64::from(ui.window().scale_factor());
                let px = |v: f64| (v * scale).round() as u32;
                (px(s.rect.2), px(s.rect.3))
            });
            if size.0 < 64 || size.1 < 64 || s.sent_size == Some(size) {
                return;
            }
            (d.console.clone(), size)
        };
        this.borrow_mut().sent_size = Some(size);
        let _ = slint::spawn_local(async move {
            if let Err(e) = console.set_ui_info(0, 0, 0, 0, size.0, size.1).await {
                eprintln!("loophole: resizing the guest desktop: {e}");
            }
        });
    }

    fn check(this: &Rc<RefCell<Self>>) {
        let refresh = {
            let s = this.borrow();
            let Some(ui) = s.ui.upgrade() else { return };
            if s.display.is_none() {
                return;
            }
            s.opts.refresh.map_or_else(
                || {
                    ui.window()
                        .with_winit_window(|w| w.current_monitor()?.refresh_rate_millihertz())
                        .flatten()
                },
                |hz| Some((hz * 1000.0).round() as u32),
            )
        };
        if this.borrow().opts.resolution.is_none() {
            Self::send_size(this);
        }
        let weak = Rc::downgrade(this);
        let _ = slint::spawn_local(async move {
            let Some(this) = weak.upgrade() else { return };
            let (vm_check, console) = {
                let s = this.borrow();
                let Some(d) = s.display.as_ref() else { return };
                (d.alive(), d.console.clone())
            };
            drop(this);
            if !vm_check.await {
                if let Some(this) = weak.upgrade() {
                    Self::gone(&this);
                }
                return;
            }
            let Some(mhz) = refresh else { return };
            let Some(this) = weak.upgrade() else { return };
            if this.borrow().sent_refresh == Some(mhz) {
                return;
            }
            this.borrow_mut().sent_refresh = Some(mhz);
            drop(this);
            match console.set_refresh_rate(mhz).await {
                Ok(()) => eprintln!(
                    "loophole: desktop updates at up to {:.2} Hz",
                    f64::from(mhz) / 1000.0
                ),
                Err(e) => eprintln!("loophole: desktop updates at QEMU's default 33 Hz: {e}"),
            }
        });
    }

    fn update_stats(&mut self) {
        let now = Instant::now();
        let secs = (now - self.stats_at).as_secs_f32().max(0.001);
        self.stats_at = now;
        let presented = std::mem::take(&mut self.presented) as f32 / secs;
        let Some(ui) = self.ui.upgrade() else { return };
        let updates = self
            .display
            .as_mut()
            .map_or(0, |d| std::mem::take(&mut d.updates));
        if !ui.get_stats_visible() {
            return;
        }
        let mut text = format!("window   {presented:5.1} frames/s presented");
        if let Some(d) = self.display.as_ref() {
            let size = d
                .source
                .as_ref()
                .map(Source::size)
                .map_or(String::new(), |(w, h)| format!(" {w}x{h}"));
            text += &format!("\nguest    {:5.1} updates/s{size}", updates as f32 / secs);
        }
        if self.opts.log_stats {
            eprintln!("loophole: {}", text.replace('\n', " | "));
        }
        ui.set_stats(text.into());
    }
}
