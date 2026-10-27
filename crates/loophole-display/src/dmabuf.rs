use std::os::fd::OwnedFd;
use std::sync::Arc;

pub struct Dmabuf {
    pub planes: Vec<(Arc<OwnedFd>, u32, u32)>,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufferKey {
    dev: u64,
    ino: u64,
    offset: u32,
    stride: u32,
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
}

impl Dmabuf {
    pub fn key(&self) -> Option<BufferKey> {
        let (fd, offset, stride) = self.planes.first()?;
        let st = rustix::fs::fstat(fd.as_ref()).ok()?;

        Some(BufferKey {
            dev: st.st_dev,
            ino: st.st_ino,
            offset: *offset,
            stride: *stride,
            width: self.width,
            height: self.height,
            fourcc: self.fourcc,
            modifier: self.modifier,
        })
    }
}
