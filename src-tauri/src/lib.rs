// Library entry point for Tauri. Used by the binary (desktop) and by the native Android/iOS app (mobile).

// Pure version logic, so it compiles (and its tests run) everywhere; the only
// caller is the Android-gated `check_android_update` command.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod android_update;
#[cfg(desktop)]
mod autostart;
// `ashpd` is declared under `[target.'cfg(target_os = "linux")'.dependencies]`,
// so this module can only compile on Linux even when the feature is enabled.
#[cfg(all(desktop, target_os = "linux", feature = "autostart-portal"))]
mod autostart_portal;
mod commands;
mod features;
mod history;
mod logging;
mod platform;
#[cfg(target_os = "android")]
mod presence_service;
mod state;
#[cfg(desktop)]
mod tray;
mod version;

pub use version::get_app_version;

use commands::*;

use state::AppState;
use std::fs;
use std::sync::Arc;

use tauri::{Emitter as _, Manager as _, RunEvent};

/// Clean up any orphaned blob store directories from previous runs. Also
/// sweeps the `.sendme-*` names older builds used, so an upgrade leaves
/// nothing behind.
fn cleanup_orphaned_directories() {
    let scan_dirs = vec![
        std::env::current_dir().ok(),
        Some(engine::storage::temp_dir()),
    ];
    for base_dir in scan_dirs.into_iter().flatten() {
        if let Ok(entries) = fs::read_dir(&base_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    let ours = name.starts_with(engine::storage::SEND_DIR_PREFIX)
                        || name.starts_with(engine::storage::RECV_DIR_PREFIX)
                        || engine::storage::LEGACY_DIR_PREFIXES
                            .iter()
                            .any(|p| name.starts_with(p));
                    if ours && entry.path().is_dir() {
                        if let Err(e) = fs::remove_dir_all(&entry.path()) {
                            tracing::warn!("Failed to clean up orphaned directory {}: {}", name, e);
                        }
                    }
                }
            }
        }
    }
}

/// Brings up the history store and reconciles rows a crash left open. Runs
/// before any command can open a row, so an `InProgress` row here is from a
/// previous process.
///
/// `cleanup_orphaned_directories` has already cleared the partial-receive
/// stores by now, so rows still pointing at one are pointing at nothing.
fn init_transfer_history(app: &tauri::App) {
    let data_dir = match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!("no app data dir; transfer history disabled: {e}");
            return;
        }
    };

    let history = Arc::new(engine::TransferHistoryStore::new(&data_dir));
    match history.mark_interrupted() {
        Ok(0) => {}
        Ok(n) => tracing::info!("marked {n} unfinished transfer(s) as interrupted"),
        Err(e) => tracing::warn!("could not reconcile unfinished transfers: {e}"),
    }
    if let Err(e) = history.forget_missing_partial_stores() {
        tracing::warn!("could not forget swept partial stores: {e}");
    }
    app.manage(history);
}

/// Entry point for both desktop (from main.rs) and mobile (from native app via mobile_entry_point).
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().plugin(tauri_plugin_store::Builder::new().build());

    // Skip in-app updater under Flatpak (`flatpak update` handles it).
    #[cfg(desktop)]
    let builder = if std::env::var_os("FLATPAK_ID").is_none() {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    } else {
        builder
    };

    #[cfg(desktop)]
    let builder = if std::env::var("ALT_SENDME_ALLOW_MULTI_INSTANCE").unwrap_or_default() == "1" {
        builder
    } else {
        builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // A duplicate autostart trigger re-invokes a running instance with
            // `--hidden`; don't force the window open then.
            if !wants_hidden_launch(args.iter().cloned()) {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            }
            let maybe_path = first_non_flag_arg(args.into_iter().skip(1));
            if let Some(path) = maybe_path {
                let app_handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app_handle.state::<state::AppStateMutex>();
                    state.lock().await.launch_intent = Some(path.clone());
                    let _ = app_handle.emit("launch-intent", path);
                });
            }
        }))
    };

    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_autostart::init(
        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
        // Autostart launches must not pop a window; see `wants_hidden_launch`.
        Some(vec!["--hidden"]),
    ));

    let builder = builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_native_utils::init())
        .manage(Arc::new(tokio::sync::Mutex::new(app_state_initial())))
        .invoke_handler(tauri::generate_handler![
            start_sharing,
            send_items,
            stop_sharing,
            receive_file,
            cancel_receive,
            list_transfer_history,
            delete_transfer_record,
            transfer_open_target,
            clear_transfer_history,
            get_transfer_temp_data,
            clear_transfer_temp_data,
            get_sharing_status,
            check_path_type,
            get_paths_mime_types,
            get_transport_status,
            get_file_size,
            #[cfg(desktop)]
            focus_main_window,
            #[cfg(desktop)]
            show_system_notification,
            #[cfg(desktop)]
            set_background_on_close,
            #[cfg(desktop)]
            set_tray_labels,
            #[cfg(desktop)]
            autostart_is_enabled,
            #[cfg(desktop)]
            autostart_set,
            check_launch_intent,
            fetch_ticket_metadata,
            verify_relays,
            verify_discovery,
            get_relay_status,
            toggle_context_menu,
            is_windows_portable,
            get_debug_logging,
            set_debug_logging,
            export_debug_bundle,
            clear_debug_logs,
            #[cfg(target_os = "android")]
            check_android_update,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            get_node_status,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            reconfigure_node_relay,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            get_device_info,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            set_device_display_name,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            get_pairing_ticket,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            start_pairing_host,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            stop_pairing_host,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            join_pairing,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            list_paired_devices,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            forget_paired_device,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            rename_paired_device,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            trust_paired_device,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            invite_paired_device,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            respond_paired_invite,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            list_nearby,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            nearby_status,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            get_discoverability,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            set_discoverability,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            invite_nearby_device,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            request_nearby_pair,
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            respond_nearby_invite,
        ])
        .setup(|app| {
            init_logging(app.handle());
            // Before Android 13 `std::env::temp_dir()` is `/data/local/tmp`,
            // which the app cannot write, so blob stores go in the app cache
            // dir instead. That is also where `TMPDIR` points on 13+, so
            // partial receives from earlier builds stay resumable. On iOS,
            // `temp_dir()` is the sandbox `tmp/`, which the system clears
            // while the app isn't running; `Library/Caches` is only purged
            // under storage pressure, so a cancelled receive can still resume.
            // Desktop keeps the unmodified `std::env::temp_dir()` fallback for
            // the same reason. Must run before `setup_common`, which scans it.
            #[cfg(any(target_os = "android", target_os = "ios"))]
            match app.path().app_cache_dir() {
                Ok(dir) => {
                    let _ = engine::storage::TEMP_DIR.set(dir);
                }
                Err(e) => tracing::error!(
                    "Could not resolve app_cache_dir; falling back to std::env::temp_dir(): {e}"
                ),
            }
            setup_common(app);
            #[cfg(desktop)]
            tray::set_background_on_close(commands::load_persisted_minimize_to_tray(
                &app.handle().clone(),
            ));
            #[cfg(desktop)]
            {
                // The window is `visible: false` so an autostart launch never
                // flashes. Shown here, ahead of node init, so relay resolution
                // can't delay it.
                //
                // Android has no equivalent show step. It stays visible only
                // because `tauri.android.conf.json` redeclares the whole
                // `app.windows` array and RFC 7396 merge *replaces* arrays, so
                // `visible: false` never reaches it. That block looks
                // redundant; it is load-bearing.
                if !wants_hidden_launch(std::env::args().skip(1)) {
                    if let Some(window) = app.get_webview_window("main") {
                        if let Err(error) = window.show() {
                            tracing::warn!(%error, "failed to show window on launch");
                        }
                    }
                }
            }
            #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
            {
                let handle = app.handle().clone();
                tauri::async_runtime::block_on(async move {
                    let state = handle.state::<state::AppStateMutex>();
                    let init_handle = handle.clone();
                    match init_node_service(init_handle).await {
                        Ok(()) => {
                            if let Err(error) = handle.emit("device-node-ready", ()) {
                                tracing::warn!(%error, "failed to emit device-node-ready");
                            }
                        }
                        Err(error) => {
                            tracing::error!(%error, "failed to initialize device node");
                            state.lock().await.node_init_error = Some(error.clone());
                            if let Err(emit_error) = handle.emit("device-node-failed", error) {
                                tracing::warn!(%emit_error, "failed to emit device-node-failed");
                            }
                        }
                    }
                });
            }
            #[cfg(desktop)]
            if let Err(error) = tray::setup_tray(&app.handle()) {
                tracing::warn!(
                    error = %error,
                    "System tray unavailable; app will continue without tray icon"
                );
            }
            #[cfg(desktop)]
            {
                use tauri::Listener as _;
                let handle = app.handle().clone();
                for event in ["paired-device-presence", "device-paired", "device-unpaired"] {
                    let handle = handle.clone();
                    app.listen(event, move |_| tray::refresh_presence(&handle));
                }
                tray::refresh_presence(&app.handle().clone());
            }
            #[cfg(target_os = "android")]
            {
                // Pairing code, Nearby, and remote unpair all land on these
                // events, so presence hooks them rather than each command.
                use tauri::Listener as _;
                let handle = app.handle().clone();
                for event in ["device-paired", "device-unpaired"] {
                    let handle = handle.clone();
                    app.listen(event, move |_| {
                        let handle = handle.clone();
                        tauri::async_runtime::spawn(async move {
                            let state = handle.state::<state::AppStateMutex>().inner().clone();
                            presence_service::refresh(&handle, &state).await;
                        });
                    });
                }
            }
            #[cfg(desktop)]
            {
                // Without a tray icon there's no way back to a hidden window.
                if wants_hidden_launch(std::env::args().skip(1)) && !tray::is_active() {
                    if let Some(window) = app.get_webview_window("main") {
                        if let Err(error) = window.show() {
                            // No tray and no window: the process is unreachable.
                            tracing::warn!(
                                %error,
                                "failed to show window after tray setup failed; app may be unreachable"
                            );
                        }
                    }
                }
            }
            Ok(())
        });

    #[cfg(desktop)]
    let builder = builder.on_window_event(|window, event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            // macOS: closing a window never quits the app. Platform
            // convention, and not something the user setting overrides.
            #[cfg(target_os = "macos")]
            {
                api.prevent_close();
                if let Err(e) = window.hide() {
                    tracing::warn!(error = %e, "failed to hide window");
                }
            }

            // Windows/Linux: hide only with a tray icon to get back from and
            // background running on. Otherwise fall through to a real close.
            #[cfg(not(target_os = "macos"))]
            {
                if !tray::is_active() || !tray::background_on_close() {
                    return;
                }
                api.prevent_close();
                tracing::debug!("App closed to system tray");
                if let Err(e) = window.hide() {
                    tracing::warn!(error = %e, "failed to hide window");
                }
            }
        }
    });

    builder
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| {
            if matches!(event, RunEvent::Exit) {
                #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
                {
                    let state = app.state::<state::AppStateMutex>();
                    tauri::async_runtime::block_on(async move {
                        let mut guard = state.lock().await;
                        if let Some(node) = guard.node.take() {
                            if let Err(error) = node.shutdown().await {
                                tracing::warn!(%error, "node shutdown error");
                            }
                        }
                    });
                }
            }
            // RunEvent::Reopen only exists on macOS (dock icon re-click)
            #[cfg(target_os = "macos")]
            if let RunEvent::Reopen { .. } = event {
                tray::open_and_focus(app);
            }
        });
}

fn first_non_flag_arg(args: impl IntoIterator<Item = String>) -> Option<String> {
    args.into_iter().find(|arg| !arg.starts_with('-'))
}

/// True when autostart launched us and no window should show. Exact match, so a
/// future `--hidden-something` can't silently trigger it.
#[cfg(desktop)]
fn wants_hidden_launch(args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter().any(|arg| arg == "--hidden")
}

fn app_state_initial() -> AppState {
    let launch_intent = first_non_flag_arg(std::env::args().skip(1));
    AppState {
        launch_intent,
        ..Default::default()
    }
}

/// Install the global tracing subscriber. Shared by the desktop binary and the
/// mobile entry point, which would otherwise have none at all.
///
/// Any failure degrades to stdout-only logging; it must never block startup.
fn init_logging(app: &tauri::AppHandle) {
    let config_dir = app.path().app_config_dir().ok();
    let log_dir = app.path().app_log_dir().ok();

    match (config_dir, log_dir) {
        (Some(config_dir), Some(log_dir)) => logging::init(&config_dir, &log_dir),
        _ => {
            // Must still install *something*, or the process ends up with no subscriber
            // and even stdout logging is lost.
            eprintln!("could not resolve app directories; debug logging unavailable");
            logging::init_stdout_only();
        }
    }

    tracing::info!(
        "Starting DashBeam application v{}",
        version::get_app_version()
    );
}

#[allow(unused_variables)]
fn setup_common(app: &tauri::App) {
    cleanup_orphaned_directories();
    init_transfer_history(app);
    tracing::debug!("File drop support enabled via dragDropEnabled config");

    #[cfg(target_os = "linux")]
    if let Some(window) = app.handle().get_webview_window("main") {
        let _ = window.set_decorations(false);
    }

    #[cfg(target_os = "windows")]
    if let Some(window) = app.handle().get_webview_window("main") {
        platform::windows::window::adjust_initial_window_size(&window);
    }
}

#[cfg(all(test, desktop))]
mod hidden_launch_tests {
    use super::wants_hidden_launch;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_the_hidden_flag() {
        assert!(wants_hidden_launch(args(&["--hidden"])));
    }

    #[test]
    fn ignores_an_absent_flag() {
        assert!(!wants_hidden_launch(args(&[])));
        assert!(!wants_hidden_launch(args(&["/Users/me/file.txt"])));
    }

    #[test]
    fn coexists_with_a_launch_intent_path() {
        // Autostart passes --hidden; a file association passes a path. Both
        // can arrive together, and neither may shadow the other.
        let both = args(&["--hidden", "/Users/me/file.txt"]);
        assert!(wants_hidden_launch(both.clone()));
        assert_eq!(
            super::first_non_flag_arg(both),
            Some("/Users/me/file.txt".to_string())
        );
    }

    #[test]
    fn does_not_match_partial_flags() {
        assert!(!wants_hidden_launch(args(&["--hidden-extra"])));
        assert!(!wants_hidden_launch(args(&["hidden"])));
    }
}
