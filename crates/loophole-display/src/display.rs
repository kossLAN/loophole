use std::os::unix::net::UnixStream;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use async_channel::Receiver;

use crate::dmabuf::{BufferKey, Dmabuf};
use crate::listener::{self, CursorShape, Event, Frame};
use crate::proxies::{ConsoleProxy, KeyboardProxy, MouseProxy, VmProxy, console_path};

pub enum Source {
    Dmabuf {
        buf: Dmabuf,
        key: Option<BufferKey>,
    },
    // pixman x8r8g8b8: bytes B, G, R, X.
    Memory {
        width: u32,
        height: u32,
        stride: u32,
        data: Vec<u8>,
    },
}

impl Source {
    pub const fn size(&self) -> (u32, u32) {
        match self {
            Self::Dmabuf { buf, .. } => (buf.width, buf.height),
            Self::Memory { width, height, .. } => (*width, *height),
        }
    }
}

#[derive(Default)]
pub struct Changed {
    pub frame: bool,
    pub cursor: Option<CursorShape>,
    pub cursor_visible: Option<bool>,
    pub cursor_at: Option<(i32, i32)>,
    pub rejected: Option<String>,
}

pub struct Display {
    _conn: zbus::Connection,
    _listener: zbus::Connection,
    vm: VmProxy<'static>,
    pub name: String,
    pub console: ConsoleProxy<'static>,
    pub label: String,
    pub source: Option<Source>,
    // GL origin: the first row is the bottom one.
    pub y0_top: bool,
    pub serial: u64,
    pub updates: u32,
    pub keyboard: KeyboardProxy<'static>,
    pub mouse: MouseProxy<'static>,
}

impl Display {
    // QEMU sends the console's current frame right away.
    pub async fn connect(stream: UnixStream) -> Result<(Self, Receiver<Event>)> {
        let conn = zbus::connection::Builder::async_io_unix_stream(stream)
            .p2p()
            .build()
            .await?;
        let vm = VmProxy::builder(&conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let name = vm.name().await.unwrap_or_default();

        let mut graphic = None;
        for id in vm.console_ids().await? {
            let proxy = ConsoleProxy::builder(&conn)
                .path(console_path(id))?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await?;
            if proxy.kind().await.unwrap_or_default() == "Graphic" {
                let label = proxy
                    .label()
                    .await
                    .unwrap_or_else(|_| format!("console {id}"));
                graphic = Some((proxy, label));
                break;
            }
        }
        let (console, label) = graphic.context("the VM has no graphic console")?;
        let path = console.inner().path().to_owned();
        let keyboard = KeyboardProxy::builder(&conn)
            .path(path.clone())?
            .build()
            .await?;
        let mouse = MouseProxy::builder(&conn).path(path)?.build().await?;
        let (tx, rx) = async_channel::unbounded();
        let listener = listener::register(&console, tx).await?;
        let display = Self {
            _conn: conn,
            _listener: listener,
            vm,
            name,
            console,
            label,
            source: None,
            y0_top: false,
            serial: 0,
            updates: 0,
            keyboard,
            mouse,
        };
        Ok((display, rx))
    }

    // Resolves to false once QEMU has gone.
    pub fn alive(&self) -> impl Future<Output = bool> + 'static {
        let vm = self.vm.clone();
        async move { vm.name().await.is_ok() }
    }

    pub fn event(&mut self, event: Event) -> Changed {
        let mut changed = Changed::default();
        match event {
            Event::Scanout(Frame::Dmabuf {
                planes,
                width,
                height,
                fourcc,
                modifier,
                y0_top,
            }) => {
                let planes = planes
                    .into_iter()
                    .map(|p| (Arc::new(p.fd), p.offset, p.stride))
                    .collect();
                let buf = Dmabuf {
                    planes,
                    width,
                    height,
                    fourcc,
                    modifier,
                };
                let key = buf.key();
                self.source = Some(Source::Dmabuf { buf, key });
                self.y0_top = y0_top;
                self.serial += 1;
                changed.frame = true;
            },
            Event::Scanout(Frame::Memory {
                width,
                height,
                stride,
                data,
            }) => {
                self.source = Some(Source::Memory {
                    width,
                    height,
                    stride,
                    data,
                });
                self.y0_top = false;
                self.serial += 1;
                changed.frame = true;
            },
            Event::Update {
                x,
                y,
                w,
                h,
                stride,
                data,
            } => {
                if let (
                    Some(Source::Memory {
                        width,
                        height,
                        stride: dst_stride,
                        data: dst,
                    }),
                    Some(rows),
                ) = (&mut self.source, data)
                {
                    let surface = Surface {
                        data: dst,
                        width: *width,
                        height: *height,
                        stride: *dst_stride,
                    };
                    surface.copy_rect((x, y, w, h), &rows, stride);
                }
                self.updates += 1;
                self.serial += 1;
                changed.frame = true;
            },
            Event::Rejected(why) => {
                changed.rejected = Some(format!("{}: not shown: {why}", self.label));
            },
            Event::Cursor(shape) => changed.cursor = Some(shape),
            Event::CursorSet { x, y, visible } => {
                changed.cursor_visible = Some(visible);
                changed.cursor_at = Some((x, y));
            },
        }
        changed
    }
}

struct Surface<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
    stride: u32,
}

impl Surface<'_> {
    fn copy_rect(self, rect: (i32, i32, i32, i32), rows: &[u8], stride: u32) {
        let clamp = |v: i32| usize::try_from(v).unwrap_or(0);
        let (x, y, w, h) = (clamp(rect.0), clamp(rect.1), clamp(rect.2), clamp(rect.3));
        let (stride, dst_stride) = (stride as usize, self.stride as usize);
        let fits = h == 0 || rows.len() >= (h - 1) * stride + w * 4;
        if !fits || x + w > self.width as usize || y + h > self.height as usize {
            return;
        }
        for r in 0..h {
            let from = r * stride;
            let to = (y + r) * dst_stride + x * 4;
            self.data[to..to + w * 4].copy_from_slice(&rows[from..from + w * 4]);
        }
    }
}
