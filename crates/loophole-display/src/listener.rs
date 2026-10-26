use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;

use anyhow::Result;
use async_channel::Sender;
use zbus::zvariant::Fd;

use crate::proxies::ConsoleProxy;

const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

pub struct Plane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

pub enum Frame {
    Dmabuf {
        planes: Vec<Plane>,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
    },
    // pixman x8r8g8b8 or a8r8g8b8.
    Memory {
        width: u32,
        height: u32,
        stride: u32,
        data: Vec<u8>,
    },
}

pub struct CursorShape {
    pub width: i32,
    pub height: i32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub data: Vec<u8>,
}

pub enum Event {
    Scanout(Frame),
    // `data` carries the pixels of a memory surface.
    Update {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        stride: u32,
        data: Option<Vec<u8>>,
    },
    Cursor(CursorShape),
    CursorSet {
        x: i32,
        y: i32,
        visible: bool,
    },
    Rejected(String),
}

#[derive(Clone)]
struct Listener {
    tx: Sender<Event>,
}

impl Listener {
    fn send(&self, e: Event) {
        let _ = self.tx.try_send(e);
    }
}

fn file_size(fd: &OwnedFd) -> Option<u64> {
    rustix::fs::seek(fd, rustix::fs::SeekFrom::End(0)).ok()
}

// Necessary, not sufficient, for a safe import: the buffer must also come with all its planes.
fn plausible(planes: &[Plane], height: u32) -> Result<(), String> {
    if planes.is_empty() || planes.len() > 4 {
        return Err(format!("{} planes", planes.len()));
    }
    for (i, p) in planes.iter().enumerate() {
        let size = file_size(&p.fd).ok_or("dma-buf size unknown")?;
        // Plane 0 holds every row. Metadata planes are smaller.
        let need = u64::from(p.offset)
            + if i == 0 {
                u64::from(p.stride) * u64::from(height)
            } else {
                1
            };
        if p.stride == 0 || need > size {
            return Err(format!(
                "plane {i}: offset {} + {} rows of {} > {size} bytes",
                p.offset, height, p.stride
            ));
        }
    }
    Ok(())
}

#[zbus::interface(name = "org.qemu.Display1.Listener")]
impl Listener {
    fn scanout(
        &self,
        width: u32,
        height: u32,
        stride: u32,
        _pixman_format: u32,
        data: serde_bytes::ByteBuf,
    ) {
        self.send(Event::Scanout(Frame::Memory {
            width,
            height,
            stride,
            data: data.into_vec(),
        }));
    }

    fn update(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        _pixman_format: u32,
        data: serde_bytes::ByteBuf,
    ) {
        self.send(Event::Update {
            x,
            y,
            w: width,
            h: height,
            stride,
            data: Some(data.into_vec()),
        });
    }

    // QEMUs without ScanoutDMABUF2 call this. It carries plane 0 only.
    #[zbus(name = "ScanoutDMABUF")]
    fn scanout_dmabuf(
        &self,
        dmabuf: Fd<'_>,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
    ) -> zbus::fdo::Result<()> {
        if modifier != DRM_FORMAT_MOD_LINEAR && modifier != DRM_FORMAT_MOD_INVALID {
            self.send(Event::Rejected(format!(
                "single-plane scanout with modifier {modifier:#x}"
            )));
            return Ok(());
        }
        let fd = dmabuf
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let planes = vec![Plane {
            fd,
            offset: 0,
            stride,
        }];
        self.scanout_planes(planes, width, height, fourcc, modifier, y0_top);
        Ok(())
    }

    // QEMU processes no command of the GPU until a listener answers.
    #[zbus(name = "UpdateDMABUF")]
    fn update_dmabuf(&self, x: i32, y: i32, width: i32, height: i32) {
        self.send(Event::Update {
            x,
            y,
            w: width,
            h: height,
            stride: 0,
            data: None,
        });
    }

    // QEMU also sends this inside every flip, between the new scanout and its update.
    #[expect(clippy::unused_self, reason = "zbus serves methods with a receiver")]
    fn disable(&self) {}

    fn mouse_set(&self, x: i32, y: i32, on: i32) {
        self.send(Event::CursorSet {
            x,
            y,
            visible: on != 0,
        });
    }

    fn cursor_define(
        &self,
        width: i32,
        height: i32,
        hot_x: i32,
        hot_y: i32,
        data: serde_bytes::ByteBuf,
    ) {
        self.send(Event::Cursor(CursorShape {
            width,
            height,
            hot_x,
            hot_y,
            data: data.into_vec(),
        }));
    }

    #[zbus(property)]
    #[expect(
        clippy::unused_self,
        reason = "zbus serves properties from methods with a receiver"
    )]
    fn interfaces(&self) -> Vec<String> {
        vec![
            "org.qemu.Display1.Listener".into(),
            "org.qemu.Display1.Listener.Unix.ScanoutDMABUF2".into(),
        ]
    }
}

impl Listener {
    fn scanout_planes(
        &self,
        planes: Vec<Plane>,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
    ) {
        match plausible(&planes, height) {
            Ok(()) => self.send(Event::Scanout(Frame::Dmabuf {
                planes,
                width,
                height,
                fourcc,
                modifier,
                y0_top,
            })),
            Err(why) => self.send(Event::Rejected(format!("{width}x{height} dma-buf: {why}"))),
        }
    }
}

struct ListenerV2(Listener);

#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.ScanoutDMABUF2")]
impl ListenerV2 {
    #[zbus(name = "ScanoutDMABUF2")]
    fn scanout_dmabuf2(
        &self,
        dmabuf: Vec<Fd<'_>>,
        _x: u32,
        _y: u32,
        width: u32,
        height: u32,
        offset: Vec<u32>,
        stride: Vec<u32>,
        num_planes: u32,
        fourcc: u32,
        _backing_width: u32,
        _backing_height: u32,
        modifier: u64,
        y0_top: bool,
    ) -> zbus::fdo::Result<()> {
        let n = num_planes as usize;
        if n == 0 || n > dmabuf.len() || n > offset.len() || n > stride.len() {
            self.0.send(Event::Rejected(format!(
                "inconsistent {num_planes}-plane scanout"
            )));
            return Ok(());
        }
        let mut planes = Vec::with_capacity(n);
        for i in 0..n {
            let fd = dmabuf[i]
                .as_fd()
                .try_clone_to_owned()
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
            planes.push(Plane {
                fd,
                offset: offset[i],
                stride: stride[i],
            });
        }
        self.0
            .scanout_planes(planes, width, height, fourcc, modifier, y0_top);
        Ok(())
    }
}

// QEMU may call either interface as soon as the connection starts.
pub async fn register(console: &ConsoleProxy<'_>, tx: Sender<Event>) -> Result<zbus::Connection> {
    let (ours, theirs) = UnixStream::pair()?;
    console.register_listener(theirs.as_fd().into()).await?;
    let listener = Listener { tx };
    let conn = zbus::connection::Builder::async_io_unix_stream(ours)
        .p2p()
        .serve_at("/org/qemu/Display1/Listener", listener.clone())?
        .serve_at("/org/qemu/Display1/Listener", ListenerV2(listener))?
        .build()
        .await?;
    Ok(conn)
}
