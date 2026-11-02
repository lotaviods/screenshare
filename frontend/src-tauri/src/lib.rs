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
            commands::set_peer_volume,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
