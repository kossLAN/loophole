use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::num::NonZeroU32;
use std::os::fd::AsRawFd as _;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use glow::HasContext as _;
use khronos_egl as egl;
use loophole_display::dmabuf::{BufferKey, Dmabuf};

const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const PLANE_FD: [i32; 4] = [0x3272, 0x3275, 0x3278, 0x3440];
const PLANE_OFFSET: [i32; 4] = [0x3273, 0x3276, 0x3279, 0x3441];
const PLANE_PITCH: [i32; 4] = [0x3274, 0x3277, 0x327A, 0x3442];
const PLANE_MODIFIER_LO: [i32; 4] = [0x3443, 0x3445, 0x3447, 0x3449];
const PLANE_MODIFIER_HI: [i32; 4] = [0x3444, 0x3446, 0x3448, 0x344A];

type CreateImage = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    u32,
    *mut c_void,
    *const i32,
) -> *mut c_void;
type DestroyImage = unsafe extern "system" fn(*mut c_void, *mut c_void) -> u32;
type ImageTargetTexture = unsafe extern "system" fn(u32, *mut c_void);

pub struct Texture {
    pub id: NonZeroU32,
    pub width: u32,
    pub height: u32,
    image: *mut c_void,
    last_used: Instant,
}

pub struct Gl {
    glow: glow::Context,
    // The library stays loaded while its function pointers are in use.
    _egl: egl::DynamicInstance<egl::EGL1_4>,
    display: *mut c_void,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_target_texture: ImageTargetTexture,
    textures: HashMap<BufferKey, Texture>,
}

impl Gl {
    pub fn new(get_proc_address: &dyn Fn(&CStr) -> *const c_void) -> Result<Self> {
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required() }
            .context("loading libEGL")?;

        let Some(display) = egl.get_current_display() else {
            bail!("the window has no EGL context (X11 with GLX?), the zero-copy display needs EGL");
        };

        let ext = |name: &str| {
            egl.get_proc_address(name)
                .with_context(|| format!("EGL has no {name}"))
        };
        let create_image = ext("eglCreateImageKHR")?;
        let destroy_image = ext("eglDestroyImageKHR")?;
        let target = get_proc_address(c"glEGLImageTargetTexture2DOES");

        if target.is_null() {
            bail!("GL has no glEGLImageTargetTexture2DOES");
        }

        let gl = unsafe { glow::Context::from_loader_function_cstr(|s| get_proc_address(s)) };

        Ok(Self {
            glow: gl,
            display: display.as_ptr(),
            create_image: unsafe {
                std::mem::transmute::<extern "system" fn(), CreateImage>(create_image)
            },
            destroy_image: unsafe {
                std::mem::transmute::<extern "system" fn(), DestroyImage>(destroy_image)
            },
            image_target_texture: unsafe {
                std::mem::transmute::<*const c_void, ImageTargetTexture>(target)
            },
            _egl: egl,
            textures: HashMap::new(),
        })
    }

    // Call with the context current.
    pub fn texture(&mut self, key: BufferKey, buf: &Dmabuf) -> Result<&Texture> {
        if !self.textures.contains_key(&key) {
            let t = self.import(buf)?;
            self.textures.insert(key, t);
        }

        let t = self.textures.get_mut(&key).expect("inserted above");
        t.last_used = Instant::now();
        Ok(t)
    }

    fn import(&mut self, buf: &Dmabuf) -> Result<Texture> {
        if buf.planes.is_empty() || buf.planes.len() > 4 {
            bail!("{} planes", buf.planes.len());
        }

        let mut attribs = vec![
            egl::WIDTH,
            buf.width as i32,
            egl::HEIGHT,
            buf.height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            buf.fourcc as i32,
        ];

        for (i, (fd, offset, stride)) in buf.planes.iter().enumerate() {
            attribs.extend([
                PLANE_FD[i],
                fd.as_raw_fd(),
                PLANE_OFFSET[i],
                *offset as i32,
                PLANE_PITCH[i],
                *stride as i32,
            ]);

            if buf.modifier != DRM_FORMAT_MOD_INVALID {
                attribs.extend([
                    PLANE_MODIFIER_LO[i],
                    (buf.modifier & 0xffff_ffff) as u32 as i32,
                    PLANE_MODIFIER_HI[i],
                    (buf.modifier >> 32) as u32 as i32,
                ]);
            }
        }

        attribs.push(egl::NONE);

        let image = unsafe {
            (self.create_image)(
                self.display,
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };

        if image.is_null() {
            bail!(
                "eglCreateImageKHR refused the {}x{} dma-buf",
                buf.width,
                buf.height
            );
        }

        unsafe {
            let tex = match self.glow.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    (self.destroy_image)(self.display, image);
                    bail!("glGenTextures: {e}");
                },
            };

            self.glow.bind_texture(glow::TEXTURE_2D, Some(tex));
            (self.image_target_texture)(glow::TEXTURE_2D, image);
            self.glow.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            self.glow.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            self.glow.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.glow.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.glow.bind_texture(glow::TEXTURE_2D, None);

            Ok(Texture {
                id: tex.0,
                width: buf.width,
                height: buf.height,
                image,
                last_used: Instant::now(),
            })
        }
    }

    pub fn evict(&mut self, keep: &[BufferKey], idle: Duration) {
        let now = Instant::now();
        let old: Vec<BufferKey> = self
            .textures
            .iter()
            .filter(|(k, t)| !keep.contains(k) && now - t.last_used > idle)
            .map(|(k, _)| *k)
            .collect();

        for k in old {
            if let Some(t) = self.textures.remove(&k) {
                self.delete(t);
            }
        }
    }

    pub fn clear(&mut self) {
        for (_, t) in std::mem::take(&mut self.textures) {
            self.delete(t);
        }
    }

    fn delete(&self, t: Texture) {
        unsafe {
            self.glow.delete_texture(glow::NativeTexture(t.id));
            (self.destroy_image)(self.display, t.image);
        }
    }
}

impl Drop for Gl {
    fn drop(&mut self) {
        self.clear();
    }
}
