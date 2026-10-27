#![expect(
    clippy::same_name_method,
    reason = "zbus::proxy generates a trait and inherent methods of the same names"
)]

use zbus::zvariant::Fd;

#[zbus::proxy(
    default_service = "org.qemu",
    interface = "org.qemu.Display1.VM",
    default_path = "/org/qemu/Display1/VM"
)]
pub trait Vm {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property, name = "ConsoleIDs")]
    fn console_ids(&self) -> zbus::Result<Vec<u32>>;
}

#[zbus::proxy(default_service = "org.qemu", interface = "org.qemu.Display1.Console")]
pub trait Console {
    fn register_listener(&self, listener: Fd<'_>) -> zbus::Result<()>;
    #[zbus(name = "SetUIInfo")]
    fn set_ui_info(
        &self,
        width_mm: u16,
        height_mm: u16,
        xoff: i32,
        yoff: i32,
        width: u32,
        height: u32,
    ) -> zbus::Result<()>;
    fn set_refresh_rate(&self, refresh_rate: u32) -> zbus::Result<()>;
    #[zbus(property)]
    fn label(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Type")]
    fn kind(&self) -> zbus::Result<String>;
}

#[zbus::proxy(default_service = "org.qemu", interface = "org.qemu.Display1.Keyboard")]
pub trait Keyboard {
    fn press(&self, keycode: u32) -> zbus::Result<()>;
    fn release(&self, keycode: u32) -> zbus::Result<()>;
}

pub mod button {
    pub const LEFT: u32 = 0;
    pub const MIDDLE: u32 = 1;
    pub const RIGHT: u32 = 2;
    pub const WHEEL_UP: u32 = 3;
    pub const WHEEL_DOWN: u32 = 4;
    pub const SIDE: u32 = 5;
    pub const EXTRA: u32 = 6;
}

#[zbus::proxy(default_service = "org.qemu", interface = "org.qemu.Display1.Mouse")]
pub trait Mouse {
    fn press(&self, button: u32) -> zbus::Result<()>;
    fn release(&self, button: u32) -> zbus::Result<()>;
    fn set_abs_position(&self, x: u32, y: u32) -> zbus::Result<()>;
    fn rel_motion(&self, dx: i32, dy: i32) -> zbus::Result<()>;
}

pub fn console_path(id: u32) -> String {
    format!("/org/qemu/Display1/Console_{id}")
}
