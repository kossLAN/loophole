use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use loophole_display::dmabuf::BufferKey;
use loophole_display::{Display, Source};
use loophole_vm::{Capture, qmp};
use slint::winit_030::WinitWindowAccessor as _;
use slint::winit_030::winit::keyboard::ModifiersState;
use slint::{ComponentHandle as _, GraphicsAPI, Image, RenderingState};

use crate::gl::Gl;
use crate::resize::Redraw;
use crate::ui::ViewerWindow;
use crate::unblock;

mod buttons;
mod frame;
mod input;

use self::buttons::Buttons;
use self::input::{Input, coalesce};

#[derive(Clone, Default)]
pub struct Options {
    pub resolution: Option<(u32, u32)>,
    pub refresh: Option<f64>,
    pub log_stats: bool,
    pub capture: Capture,
}

fn fit(w: f64, h: f64, fw: f64, fh: f64) -> (f64, f64, f64, f64) {
    let s = (w / fw).min(h / fh);
    let (dw, dh) = (fw * s, fh * s);
    ((w - dw) / 2.0, (h - dh) / 2.0, dw, dh)
}

pub struct Screen {
    me: Weak<RefCell<Self>>,
    ui: slint::Weak<ViewerWindow>,
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
    rect: (f64, f64, f64, f64),
    host: Option<(f64, f64)>,
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
    motion: (f64, f64),
    keys_down: BTreeSet<u32>,
    modifiers: ModifiersState,

    resize_timer: slint::Timer,
    check_timer: slint::Timer,
    stats_timer: slint::Timer,
    sent_size: Option<(u32, u32)>,
    sent_refresh: Option<u32>,

    presented: u32,
    redraw: Redraw,
    stats_at: Instant,
}

thread_local! {
    static SCREENS: RefCell<Vec<Weak<RefCell<Screen>>>> = const { RefCell::new(Vec::new()) };
}

pub fn raw_motion(dx: f64, dy: f64) {
    let screens: Vec<_> = SCREENS.with(|s| {
        let mut s = s.borrow_mut();
        s.retain(|w| w.strong_count() > 0);
        s.iter().filter_map(Weak::upgrade).collect()
    });

    for s in screens {
        s.borrow_mut().raw_motion(dx, dy);
    }
}

impl Screen {
    pub fn install(ui: &ViewerWindow, opts: Options) -> Rc<RefCell<Self>> {
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
                redraw: Redraw::default(),
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
                RenderingState::AfterRendering => {
                    s.presented += 1;

                    if let Some(ui) = s.ui.upgrade() {
                        s.redraw.after_render(ui.window());
                    }
                },
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
        SCREENS.with(|s| s.borrow_mut().push(Rc::downgrade(&this)));

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
                        ui.set_vm_name(display.name.as_str().into());
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
        s.update_hint();

        if let Some(ui) = s.ui.upgrade() {
            ui.set_frame(Image::default());
            ui.set_cursor_visible(false);
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

    pub fn update_hint(&self) {
        let Some(ui) = self.ui.upgrade() else { return };

        let hint = if self.display.is_none() {
            ""
        } else if self.captured {
            "Keyboard and pointer → guest  Ctrl+Alt+G takes them back"
        } else if !self.keyboard_to_guest {
            "Keyboard → loophole  Ctrl+Alt+G gives it to the guest"
        } else if self.opts.capture == Capture::Manual {
            "Keyboard → guest  Ctrl+Alt+G captures the pointer  Ctrl+Alt+F fullscreen  Ctrl+Alt+S stats"
        } else {
            "Keyboard → guest  Ctrl+Alt+G takes it back  Ctrl+Alt+F fullscreen  Ctrl+Alt+S stats"
        };

        ui.set_hint(hint.into());
    }

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
