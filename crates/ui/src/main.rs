mod app;
mod backend;
mod text_input;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, TitlebarOptions, WindowBounds,
    WindowOptions, px, size,
};
use sharescreen_config::ConfigStore;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use app::AppView;
use text_input::{
    Backspace, Copy, Cut, Delete, End, Home, Left, Paste, Right, SelectAll, SelectLeft,
    SelectRight, ShowCharacterPalette,
};

/// A `Write` sink shared by every log event so the log file stays open.
struct LogFile(Arc<Mutex<std::fs::File>>);

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut file) => file.write(buf),
            // Never let logging take the app down.
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock() {
            Ok(mut file) => file.flush(),
            Err(_) => Ok(()),
        }
    }
}

/// Log to stdout and, when a config directory exists, to a file beside
/// `config.toml`. The Windows GUI build has no console, so the file is the
/// only place a user can look after a failed connect.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Only emit ANSI colors when stdout is a real terminal and the user has
    // not asked for plain output. When output is captured or redirected, the
    // escape codes would show up as garbage.
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    let console = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_ansi(std::io::stdout().is_terminal() && !no_color);

    let mut log_path = None;
    let file_layer = match sharescreen_config::default_log_path() {
        Some(path) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(file) => {
                    let file = Arc::new(Mutex::new(file));
                    log_path = Some(path);
                    Some(
                        tracing_subscriber::fmt::layer()
                            .with_target(true)
                            .with_ansi(false)
                            .with_writer(move || LogFile(Arc::clone(&file))),
                    )
                }
                Err(error) => {
                    eprintln!("failed to open log file {}: {error}", path.display());
                    None
                }
            }
        }
        None => None,
    };

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .try_init();

    if let Some(path) = log_path {
        tracing::info!(path = %path.display(), "logging to file");
    }
}

fn main() {
    init_logging();

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
