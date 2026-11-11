mod app;
mod backend;
mod text_input;

use std::path::PathBuf;

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, TitlebarOptions, WindowBounds,
    WindowOptions, px, size,
};
use sharescreen_config::ConfigStore;

use app::AppView;
use text_input::{
    Backspace, Copy, Cut, Delete, End, Home, Left, Paste, Right, SelectAll, SelectLeft,
    SelectRight, ShowCharacterPalette,
};

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();

    #[cfg(target_os = "linux")]
    {
        let missing = sharescreen_capture::missing_elements();
        if missing.is_empty() {
            tracing::info!("GStreamer runtime complete");
        } else {
            tracing::error!(
                "GStreamer is missing required elements, capture/audio/decode will fail: {}",
                missing.join(", ")
            );
        }
    }

    let store = ConfigStore::load_or_default().unwrap_or_else(|error| {
        tracing::error!("failed to load configuration: {error}");
        ConfigStore::load_from_path(PathBuf::from("sharescreen-config.toml"))
            .expect("failed to write fallback configuration")
    });

    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, None),
            KeyBinding::new("delete", Delete, None),
            KeyBinding::new("left", Left, None),
            KeyBinding::new("right", Right, None),
            KeyBinding::new("shift-left", SelectLeft, None),
            KeyBinding::new("shift-right", SelectRight, None),
            KeyBinding::new("cmd-a", SelectAll, None),
            KeyBinding::new("cmd-v", Paste, None),
            KeyBinding::new("cmd-c", Copy, None),
            KeyBinding::new("cmd-x", Cut, None),
            KeyBinding::new("home", Home, None),
            KeyBinding::new("end", End, None),
            KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, None),
        ]);

        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Sharescreen".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(900.), px(600.))),
                ..Default::default()
            },
            move |_, cx| cx.new(|cx| AppView::new(store, cx)),
        )
        .expect("failed to open the Sharescreen window");
        cx.activate(true);
    });
}
