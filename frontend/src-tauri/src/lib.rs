mod commands;
mod state;

use sharescreen_audio::AudioEngine;
use sharescreen_capture::CaptureEngine;
use sharescreen_config::ConfigStore;
use sharescreen_core::CaptureBackend;
use tauri::Manager;

use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Without a subscriber every `tracing` log in the crates is dropped, which
    // hides media/capture failures. Default to `info`; override with RUST_LOG.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();

    #[cfg(windows)]
    configure_bundled_gstreamer();

    let missing = sharescreen_capture::missing_elements();
    if missing.is_empty() {
        tracing::info!("GStreamer runtime complete");
    } else {
        tracing::error!(
            "GStreamer is missing required elements, capture/audio/decode will fail: {}",
            missing.join(", ")
        );
    }

    tauri::Builder::default()
        .setup(|app| {
            let config = ConfigStore::load_or_default().map_err(|e| e.to_string())?;
            let backend = config.config().capture.backend;
            let capture = CaptureEngine::new(backend).unwrap_or_else(|_| {
                CaptureEngine::new(CaptureBackend::Auto).expect("auto capture backend is available")
            });
            app.manage(AppState {
                config: std::sync::Mutex::new(config),
                audio: tokio::sync::Mutex::new(AudioEngine::default()),
                capture: tokio::sync::Mutex::new(capture),
                call: tokio::sync::Mutex::new(None),
                share: std::sync::Mutex::new(None),
                mic_running: std::sync::atomic::AtomicBool::new(false),
                app: app.handle().clone(),
            });
            commands::spawn_hotplug_watcher(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::update_config,
            commands::list_microphones,
            commands::list_outputs,
            commands::list_system_monitors,
            commands::list_capture_sources,
            commands::start_screen_share,
            commands::stop_screen_share,
            commands::start_microphone,
            commands::stop_microphone,
            commands::start_system_audio,
            commands::stop_system_audio,
            commands::set_mic_volume,
            commands::set_mic_muted,
            commands::set_system_volume,
            commands::connect_server,
            commands::disconnect_server,
            commands::create_room,
            commands::join_room,
            commands::leave_room,
            commands::delete_room,
            commands::local_frame,
            commands::remote_frame,
            commands::pip_frame,
            commands::open_pip_window,
            commands::close_pip_window,
            commands::is_connected,
            commands::get_current_room,
            commands::set_peer_volume,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Make the portable Windows build use the GStreamer runtime shipped beside
/// the executable. A normal developer install continues to use its existing
/// GStreamer environment when the bundled runtime is absent.
#[cfg(windows)]
fn configure_bundled_gstreamer() {
    use std::{env, path::PathBuf};

    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(app_dir) = exe.parent() else {
        return;
    };
    let gst_dir = app_dir.join("gstreamer");
    let plugins_dir = gst_dir.join("lib/gstreamer-1.0");
    if !plugins_dir.is_dir() {
        return;
    }

    let bin_dir = PathBuf::from(app_dir);
    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin_dir];
    paths.extend(env::split_paths(&path));
    if let Ok(path) = env::join_paths(paths) {
        env::set_var("PATH", path);
    }
    env::set_var("GST_PLUGIN_PATH_1_0", &plugins_dir);
    env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", &plugins_dir);
    env::set_var("GST_PLUGIN_SCANNER", app_dir.join("gst-plugin-scanner.exe"));
    env::set_var("GST_PLUGIN_SCANNER_1_0", app_dir.join("gst-plugin-scanner.exe"));
    env::set_var("GST_PLUGIN_PATH", &plugins_dir);
    env::set_var("GST_PLUGIN_SYSTEM_PATH", &plugins_dir);
    env::set_var("GST_REGISTRY", app_dir.join("gstreamer-registry.bin"));
}
