use std::time::Duration;

use loophole_display::Source;
use loophole_display::listener::{CursorShape, Event};
use slint::{
    BorrowedOpenGLTextureBuilder, BorrowedOpenGLTextureOrigin, ComponentHandle as _, Image,
    Rgba8Pixel, SharedPixelBuffer,
};

use super::{Screen, fit};

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

impl Screen {
    pub(super) fn event(&mut self, e: Event) {
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

    pub(super) fn before_render(&mut self) {
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

    pub(super) fn frame_size(&self) -> Option<(u32, u32)> {
        Some(self.display.as_ref()?.source.as_ref()?.size())
    }

    pub(super) fn update_cursor(&self) {
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

    pub(super) fn geometry(&mut self, x: f32, y: f32, w: f32, h: f32) {
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
}
