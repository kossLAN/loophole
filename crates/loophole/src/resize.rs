use slint::{ComponentHandle, PhysicalSize, RenderingState};

// Slint 1.18 makes a window's GL context current before resizing its surface
#[derive(Default)]
pub struct Redraw(Option<PhysicalSize>);

impl Redraw {
    pub fn after_render(&mut self, window: &slint::Window) {
        let size = window.size();

        if self.0.replace(size).is_some_and(|last| last != size) {
            window.request_redraw();
        }
    }
}

pub fn redraw_after_resize<C>(ui: &C)
where
    C: ComponentHandle + 'static,
{
    let weak = ui.as_weak();
    let mut redraw = Redraw::default();
    let _ = ui.window().set_rendering_notifier(move |state, _| {
        if let (RenderingState::AfterRendering, Some(ui)) = (state, weak.upgrade()) {
            redraw.after_render(ui.window());
        }
    });
}
