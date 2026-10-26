use ashpd::desktop::settings::{ColorScheme, Settings};
use slint::ComponentHandle as _;

use crate::ui::{MainWindow, Theme};

// The desktop's dark/light preference through the XDG portal. Dark when there is none.
pub fn follow(ui: &MainWindow) {
    let weak = ui.as_weak();
    let _ = slint::spawn_local(async move {
        let Ok(settings) = Settings::new().await else {
            return;
        };
        if let (Ok(scheme), Some(ui)) = (settings.color_scheme().await, weak.upgrade()) {
            ui.global::<Theme>()
                .set_dark(scheme != ColorScheme::PreferLight);
        }
    });
}
