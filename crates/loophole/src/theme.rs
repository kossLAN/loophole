use std::cell::{Cell, RefCell};
use std::pin::pin;

use ashpd::desktop::settings::{ColorScheme, Settings};
use futures_util::StreamExt as _;
use slint::ComponentHandle;

use crate::ui::{Theme, TrayIcon};

type Apply = Box<dyn Fn(bool) -> bool>;

thread_local! {
    static WINDOWS: RefCell<Vec<Apply>> = const { RefCell::new(Vec::new()) };
    static DARK: Cell<Option<bool>> = const { Cell::new(None) };
    static WATCHING: Cell<bool> = const { Cell::new(false) };
}

pub fn follow<W>(ui: &W)
where
    W: ComponentHandle + 'static,
    for<'a> Theme<'a>: slint::Global<'a, W>,
{
    let weak = ui.as_weak();
    register(Box::new(move |dark| {
        weak.upgrade()
            .map(|ui| ui.global::<Theme>().set_dark(dark))
            .is_some()
    }));
}

pub fn follow_tray(tray: &TrayIcon) {
    let weak = tray.as_weak();
    register(Box::new(move |dark| {
        weak.upgrade()
            .map(|t| t.global::<Theme>().set_dark(dark))
            .is_some()
    }));
}

fn register(apply: Apply) {
    if let Some(dark) = DARK.get() {
        apply(dark);
    }

    WINDOWS.with_borrow_mut(|w| w.push(apply));

    if !WATCHING.replace(true) {
        let _ = slint::spawn_local(watch());
    }
}

fn set(scheme: ColorScheme) {
    let dark = scheme != ColorScheme::PreferLight;
    DARK.set(Some(dark));
    WINDOWS.with_borrow_mut(|w| w.retain(|apply| apply(dark)));
}

async fn watch() {
    let Ok(settings) = Settings::new().await else {
        return;
    };

    if let Ok(scheme) = settings.color_scheme().await {
        set(scheme);
    }

    let Ok(changes) = settings.receive_color_scheme_changed().await else {
        return;
    };

    let mut changes = pin!(changes);

    while let Some(scheme) = changes.next().await {
        set(scheme);
    }
}
