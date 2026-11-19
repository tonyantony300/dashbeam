use crate::features::thumbnail::generate_thumbnail;
use crate::history::{
    history_enabled, partial_store_path_for, CompletionFacts, HistoryRecordingEmitter,
    TransferContext,
};
use crate::state::{AppStateMutex, ShareHandle};
use engine::{
    build_discovery_mode, download, fetch_metadata, get_relay_status as engine_get_relay_status,
    resolve_relay_mode_with_fallback, start_share_items,
    verify_discovery as engine_verify_discovery, verify_relays as engine_verify_relays,
    AddrInfoOptions, AppHandle, DeviceInfo, Discoverability, EventEmitter, FileMetadata,
    FilePreviewItem, NearbyDevice, NodeService, PairedDevice, PairedDeviceInfo, ReceiveOptions,
    SendOptions, TransferDirection, TransferHistoryStore, TransferPathType, TransferPeer,
    TransferRecord, TransferStatus,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};

#[allow(unused_imports)]
pub use engine::{
    build_relay_mode, relay_fallback_policy, DiscoveryConfigArg, RelayConfigArg,
    RelayFallbackPolicy, RelayStatusResponse, VerifyDiscoveryResponse, VerifyRelaysResponse,
};

fn relay_fallback_event_payload(
    stage: &'static str,
    fell_back_to_public: bool,
) -> Option<&'static str> {
    fell_back_to_public.then_some(stage)
}

/// Check which relay the app can reach, with public fallback only when selected.
#[tauri::command]
pub async fn get_relay_status(
    relay: Option<RelayConfigArg>,
) -> Result<RelayStatusResponse, String> {
    engine_get_relay_status(relay).await
}

// Wrapper for Tauri AppHandle that implements EventEmitter
struct TauriEventEmitter {
    app_handle: tauri::AppHandle,
}

/// Builds the emitter the engine reports through, wrapping it in a history
/// recorder unless the user turned recording off.
///
/// Returns the recorder separately so cancel paths, which see no engine event,
/// can still close the row.
fn build_emitter(
    app_handle: &tauri::AppHandle,
    history: &State<'_, Arc<TransferHistoryStore>>,
    direction: TransferDirection,
    context: TransferContext,
) -> (AppHandle, Option<Arc<HistoryRecordingEmitter>>) {
    let base: Arc<dyn engine::EventEmitter> = Arc::new(TauriEventEmitter {
        app_handle: app_handle.clone(),
    });

    let Ok(data_dir) = app_handle.path().app_data_dir() else {
        return (Some(base), None);
    };
    if !history_enabled(&data_dir) {
        return (Some(base), None);
    }

    let recorder = Arc::new(HistoryRecordingEmitter::new(
        base,
        history.inner().clone(),
        direction,
        context,
    ));
    (Some(recorder.clone()), Some(recorder))
}

/// The node service, if pairing came up. Its absence is not fatal — history
/// just records an endpoint id with no name.
#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
async fn app_state_node(state: &State<'_, AppStateMutex>) -> Option<Arc<NodeService>> {
    state.lock().await.node.clone()
}

#[cfg(not(any(desktop, target_os = "android", target_os = "ios")))]
async fn app_state_node(_state: &State<'_, AppStateMutex>) -> Option<Arc<NodeService>> {
    None
}

/// Names an endpoint id from the paired-device list. The snapshot is stored
/// alongside the id so a forgotten device keeps its name in past rows.
fn name_peer(endpoint_id: String, node: &Option<Arc<NodeService>>) -> TransferPeer {
    let known = node
        .as_ref()
        .and_then(|node| node.list_paired().ok())
        .and_then(|devices| {
            devices
                .into_iter()
                .find(|d| d.endpoint_id.eq_ignore_ascii_case(&endpoint_id))
        });

    match known {
        Some(device) => TransferPeer {
            endpoint_id,
            display_name: Some(device.display_name),
            device_type: Some(device.device_type),
        },
        None => TransferPeer {
            endpoint_id,
            display_name: None,
            device_type: None,
        },
    }
}

/// The sender behind a ticket. Every ticket carries its origin endpoint id, so
/// this works for a pasted ticket as well as a paired invite.
fn sender_peer_from_ticket(ticket: &str, node: &Option<Arc<NodeService>>) -> Option<TransferPeer> {
    use iroh_blobs::ticket::BlobTicket;
    use std::str::FromStr;

    let ticket = BlobTicket::from_str(ticket).ok()?;
    let endpoint_id = ticket.addr().id.to_string();
    Some(name_peer(endpoint_id, node))
}

/// Attributes the active share to a device, when the UI targeted one.
fn note_share_peer(app_state: &crate::state::AppState, peer: TransferPeer) {
    if let Some(recorder) = app_state
        .current_share
        .as_ref()
        .and_then(|share| share.recorder.as_ref())
    {
        recorder.note_invited_peer(peer);
    }
}

fn path_type_from_mime(mime: Option<&str>) -> Option<TransferPathType> {
    match mime {
        Some("inode/directory") => Some(TransferPathType::Directory),
        Some(_) => Some(TransferPathType::File),
        None => None,
    }
}

impl EventEmitter for TauriEventEmitter {
    fn emit_event(&self, event_name: &str) -> Result<(), String> {
        self.app_handle
            .emit(event_name, ())
            .map_err(|e| e.to_string())
    }

    fn emit_event_with_payload(&self, event_name: &str, payload: &str) -> Result<(), String> {
        self.app_handle
            .emit(event_name, payload)
            .map_err(|e| e.to_string())
    }
}

/// Get file or directory size
#[tauri::command]
pub async fn get_file_size(path: String) -> Result<u64, String> {
    let path = PathBuf::from(path);

    if !path.exists() {
        return Err("Path does not exist".to_string());
    }

    tokio::task::spawn_blocking(move || get_total_size(&path))
        .await
        .map_err(|e| format!("Task join error: {}", e))?
}

#[tauri::command]
#[cfg(desktop)]
pub async fn focus_main_window(app_handle: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app_handle.get_webview_window("main") {
        window.show().map_err(|e| e.to_string())?;
        if window.is_minimized().map_err(|e| e.to_string())? {
            window.unminimize().map_err(|e| e.to_string())?;
        }
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }

    if let Some(window) = app_handle.webview_windows().values().next() {
        window.show().map_err(|e| e.to_string())?;
        if window.is_minimized().map_err(|e| e.to_string())? {
            window.unminimize().map_err(|e| e.to_string())?;
        }
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }

    Err("No window available to focus".to_string())
}

#[tauri::command]
pub async fn start_sharing(
    path: String,
    relay: Option<RelayConfigArg>,
    discovery: Option<DiscoveryConfigArg>,
    state: State<'_, AppStateMutex>,
    history: State<'_, Arc<TransferHistoryStore>>,
    app_handle: tauri::AppHandle,
) -> Result<String, String> {
    send_items(vec![path], relay, discovery, state, history, app_handle).await
}

/// New interface to start_sharing multiple items at once
#[tauri::command]
pub async fn send_items(
    paths: Vec<String>,
    relay: Option<RelayConfigArg>,
    discovery: Option<DiscoveryConfigArg>,
    state: State<'_, AppStateMutex>,
    history: State<'_, Arc<TransferHistoryStore>>,
    app_handle: tauri::AppHandle,
) -> Result<String, String> {
    // Validate input before doing any work.
    if paths.is_empty() {
        return Err("No paths provided".to_string());
    }

    let path_bufs: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();

    // Reserve slot before expensive setup to avoid concurrent start_sharing races.
    {
        let mut app_state = state.lock().await;
        if app_state.current_share.is_some() || app_state.is_share_starting {
            return Err("Already sharing a file. Please stop current share first.".to_string());
        }
        app_state.is_share_starting = true;
    }

    let start_result = async {
        // Prepare metadata outside the state mutex.
        let metadata = build_send_metadata(&path_bufs).await?;
        tracing::info!(
            first_path_stem = ?path_bufs[0].file_stem(),
            total_size = metadata.size,
            has_thumbnail = metadata.thumbnail.is_some(),
            "share metadata prepared for multiple items"
        );

        // Create send options from relay settings.
        let (relay_mode, fell_back_to_public) = resolve_relay_mode_with_fallback(relay).await?;
        let discovery_mode = build_discovery_mode(discovery)?;
        let options = SendOptions {
            relay_mode,
            discovery_mode,
            ticket_type: AddrInfoOptions::RelayAndAddresses,
            magic_ipv4_addr: None,
            magic_ipv6_addr: None,
        };

        // Known before the first byte moves; the row opens on first pull.
        let (boxed_handle, recorder) = build_emitter(
            &app_handle,
            &history,
            TransferDirection::Send,
            TransferContext {
                root_name: metadata.file_name.clone(),
                payload_bytes: metadata.size,
                item_count: metadata.item_count,
                path_type: path_type_from_mime(metadata.mime_type.as_deref()),
                ..Default::default()
            },
        );

        // Ephemeral share — relay settings apply per session (all platforms including Android).
        let result = start_share_items(path_bufs.clone(), options, &boxed_handle, Some(metadata))
            .await
            .map_err(|error| {
                // Without this, sender-side bug reports end at connection setup.
                tracing::error!(
                    target: "dashbeam::_events::transfer::send_failed",
                    item_count = path_bufs.len(),
                    %error,
                );
                format!("Failed to start sharing: {}", error)
            })?;
        if let Some(payload) = relay_fallback_event_payload("send", fell_back_to_public) {
            // Surface the selected custom->public fallback once the share has
            // actually started with the resolved relay mode.
            let _ = app_handle.emit("relay-fell-back", payload);
        }
        if let Some(recorder) = recorder.as_ref() {
            let hash = result.hash.clone();
            recorder.update_context(|context| context.blob_hash = Some(hash));
        }
        Ok((result.ticket.clone(), path_bufs, result, recorder))
    }
    .await;

    match start_result {
        Ok((ticket, paths, result, recorder)) => {
            let mut app_state = state.lock().await;
            app_state.is_share_starting = false;

            if app_state.current_share.is_some() {
                return Err("Already sharing a file. Please stop current share first.".to_string());
            }

            // Keep full send result alive to preserve router/temp_tag lifecycle.
            let primary = paths.first().cloned().unwrap_or_else(|| PathBuf::from("."));
            app_state.current_share =
                Some(ShareHandle::new(ticket.clone(), primary, result, recorder));
            Ok(ticket)
        }
        Err(e) => {
            let mut app_state = state.lock().await;
            app_state.is_share_starting = false;
            Err(e)
        }
    }
}

async fn build_send_metadata(paths: &[PathBuf]) -> Result<FileMetadata, String> {
    if paths.is_empty() {
        return Err("No paths provided".to_string());
    }

    let total_size = {
        let paths_for_size = paths.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut total = 0u64;
            for path in &paths_for_size {
                total = total.saturating_add(get_total_size(path)?);
            }
            Ok::<u64, String>(total)
        })
        .await
        .map_err(|e| format!("Task join error: {}", e))??
    };

    if paths.len() == 1 {
        let path = &paths[0];
        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();

        let thumbnail = generate_thumbnail(path).await;
        let mime_type = if path.is_file() {
            Some(
                mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_string(),
            )
        } else {
            Some("inode/directory".to_string())
        };

        return Ok(FileMetadata {
            file_name,
            item_count: 1,
            size: total_size,
            thumbnail,
            mime_type,
            items: None,
        });
    }

    // For multiple items
    let first_name = paths[0]
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let preview_items = collect_preview_items(paths).await?;
    let thumbnail = preview_items.iter().find_map(|item| item.thumbnail.clone());

    Ok(FileMetadata {
        file_name: first_name,
        item_count: paths.len() as u32,
        size: total_size,
        thumbnail,
        mime_type: Some("application/x-iroh-collection".to_string()),
        items: Some(preview_items),
    })
}

/// Fetch metadata from sender by ticket, without starting file download.
#[tauri::command]
pub async fn fetch_ticket_metadata(
    ticket: String,
    relay: Option<RelayConfigArg>,
    discovery: Option<DiscoveryConfigArg>,
) -> Result<FileMetadata, String> {
    let ticket_len = ticket.len();
    tracing::info!(ticket_len, "fetch_ticket_metadata called");

    let (relay_mode, _) = resolve_relay_mode_with_fallback(relay).await?;
    let discovery_mode = build_discovery_mode(discovery)?;
    let options = ReceiveOptions {
        output_dir: None,
        relay_mode,
        discovery_mode,
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
    };

    match fetch_metadata(ticket, options).await {
        Ok(metadata) => {
            tracing::info!(
                file_name_len = metadata.file_name.len(),
                size = metadata.size,
                has_thumbnail = metadata.thumbnail.is_some(),
                "fetch_ticket_metadata succeeded"
            );
            Ok(metadata)
        }
        Err(e) => Err(format!("Failed to fetch metadata: {}", e)),
    }
}

/// Stop the current sharing session
#[tauri::command]
pub async fn stop_sharing(state: State<'_, AppStateMutex>) -> Result<(), String> {
    let mut app_state = state.lock().await;

    if let Some(mut share) = app_state.current_share.take() {
        if let Some(recorder) = share.recorder.as_ref() {
            // No-op if a terminal event already closed the row.
            recorder.finalize(TransferStatus::Cancelled, CompletionFacts::default(), None);
        }
        if let Err(e) = share.stop().await {
            return Err(e);
        }

        #[cfg(target_os = "android")]
        let _ = std::fs::remove_dir_all(&share._path);
    }

    #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
    if let Some(node) = app_state.node.as_ref() {
        node.stop_pairing_host().await;
    }

    Ok(())
}

/// Receive a file using a ticket
#[tauri::command]
pub async fn receive_file(
    ticket: String,
    output_path: String,
    tree_uri: Option<String>,
    tree_display_path: Option<String>,
    sub_folder: Option<String>,
    relay: Option<RelayConfigArg>,
    discovery: Option<DiscoveryConfigArg>,
    state: State<'_, AppStateMutex>,
    history: State<'_, Arc<TransferHistoryStore>>,
    app_handle: tauri::AppHandle,
) -> Result<String, String> {
    use iroh_blobs::ticket::BlobTicket;
    use std::str::FromStr;

    // Derive the content hash so we can manage partial store lifecycle. Parsed
    // up front because the folder sanitizer also needs a per-peer fallback, and
    // the sender's endpoint id rides in the same ticket.
    let incoming_hash = BlobTicket::from_str(&ticket)
        .ok()
        .map(|t| t.hash().to_hex().to_string());
    let folder_fallback = BlobTicket::from_str(&ticket)
        .ok()
        .map(|t| t.addr().id.to_string().chars().take(12).collect::<String>())
        .unwrap_or_else(|| "Device".to_string());

    // Auto-accepted transfers file themselves under the sender's name. The name
    // is chosen by the peer, so it must be sanitized before it becomes a path
    // component — see `sanitize_folder_name`.
    let sanitized_sub_folder = sub_folder
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| engine::sanitize_folder_name(name, &folder_fallback));

    #[cfg(target_os = "android")]
    let (output_dir, export_root) = {
        let _ = &output_path;
        resolve_android_receive_dirs(&app_handle, sanitized_sub_folder.as_deref())?
    };
    #[cfg(not(target_os = "android"))]
    let output_dir =
        resolve_receive_output_dir(&app_handle, output_path, sanitized_sub_folder.as_deref())?;
    let (relay_mode, fell_back_to_public) = resolve_relay_mode_with_fallback(relay).await?;
    let discovery_mode = build_discovery_mode(discovery)?;
    let options = ReceiveOptions {
        output_dir: Some(output_dir.clone()),
        relay_mode,
        discovery_mode,
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
    };

    // If a previous cancel left a partial store for a *different* hash, delete it now.
    // Same hash → keep it for resume. Different hash → it would never be reused.
    // Only act when we know the new hash; if the ticket is unparseable, leave the
    // stale entry intact so the next valid attempt can still clean it up.
    if let Some(ref new_hash) = incoming_hash {
        let stale_hash = state.lock().await.last_cancelled_recv_hash.take();
        if let Some(stale_hash) = stale_hash {
            if &stale_hash != new_hash {
                let stale_dir = engine::storage::temp_dir()
                    .join(format!("{}{stale_hash}", engine::storage::RECV_DIR_PREFIX));
                if stale_dir.exists() {
                    if let Err(e) = tokio::fs::remove_dir_all(&stale_dir).await {
                        tracing::warn!("Failed to remove stale partial recv store: {}", e);
                    } else {
                        tracing::info!("Removed stale partial recv store for hash {}", stale_hash);
                    }
                }
            }
        }
    }

    // The sender's endpoint id rides in the ticket, so the peer is known before
    // the connection exists. `resumable_store_path` is set at open, not finalize
    // — a crash never reaches finalize, stranding the partial store.
    let sender_peer = sender_peer_from_ticket(&ticket, &app_state_node(&state).await);
    // Android receives into app-private staging and only learns where the files
    // really land when the export finishes, so the row stays silent until
    // `note_destination` fills it in. Recording staging here would leave every
    // failed export pointing at a path no file manager can reach.
    #[cfg(target_os = "android")]
    let recorded_save_path: Option<String> = None;
    #[cfg(not(target_os = "android"))]
    let recorded_save_path = Some(output_dir.to_string_lossy().to_string());
    let (boxed_handle, recorder) = build_emitter(
        &app_handle,
        &history,
        TransferDirection::Receive,
        TransferContext {
            blob_hash: incoming_hash.clone(),
            save_path: recorded_save_path,
            resumable_store_path: incoming_hash
                .as_deref()
                .map(|hash| partial_store_path_for(hash).to_string_lossy().to_string()),
            peer: sender_peer,
            ..Default::default()
        },
    );

    if let Some(payload) = relay_fallback_event_payload("receive", fell_back_to_public) {
        let _ = app_handle.emit("relay-fell-back", payload);
    }

    // Create a cancel channel and store the sender so cancel_receive can fire it.
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut app_state = state.lock().await;
        if app_state.current_receive_cancel.is_some() {
            return Err(
                "Already receiving a file. Wait for the current download to finish.".to_string(),
            );
        }
        app_state.current_receive_cancel = Some(cancel_tx);
        app_state.current_receive_hash = incoming_hash.clone();
    }

    let result = download(ticket, options, boxed_handle, cancel_rx).await;

    // Update state based on outcome.
    {
        let mut app_state = state.lock().await;
        app_state.current_receive_cancel = None;
        app_state.current_receive_hash = None;
        match &result {
            Err(e) if e.to_string() == "cancelled" => {
                // Record the hash so the next receive can decide whether to delete this partial.
                app_state.last_cancelled_recv_hash = incoming_hash;
            }
            Ok(_) | Err(_) => {
                // Success deletes the store automatically (armed guard).
                // Network errors keep the partial for same-session retry — treated the
                // same as cancel from the user's perspective re: cleanup.
                if result.is_err() {
                    app_state.last_cancelled_recv_hash = incoming_hash;
                }
            }
        }
    }

    if let Some(recorder) = recorder.as_ref() {
        match &result {
            // Success already closed the row from `receive-completed`.
            Ok(_) => {}
            Err(e) if e.to_string() == "cancelled" => {
                recorder.finalize(TransferStatus::Cancelled, CompletionFacts::default(), None)
            }
            Err(e) => recorder.finalize(
                TransferStatus::Failed,
                CompletionFacts::default(),
                Some(e.to_string()),
            ),
        }
    }

    match result {
        Ok(r) => {
            #[cfg(target_os = "android")]
            {
                let destination = finalize_android_receive(
                    &app_handle,
                    AndroidExport {
                        root: &export_root,
                        files_dir: &output_dir,
                        tree_uri: tree_uri.as_deref(),
                        tree_display_path: tree_display_path.as_deref(),
                    },
                )?;
                // History opened the row against staging, which the export just
                // emptied. The subfolder is part of the answer: the exporter
                // recreates the staging tree under the destination.
                if let (Some(recorder), Some(destination)) = (recorder.as_ref(), destination) {
                    let display = match sanitized_sub_folder.as_deref() {
                        Some(name) => format!("{}/{name}", destination.display),
                        None => destination.display,
                    };
                    recorder.note_destination(display, destination.uri);
                }
            }
            #[cfg(not(target_os = "android"))]
            {
                let _ = (tree_uri, tree_display_path);
            }
            Ok(r.message)
        }
        Err(e) if e.to_string() == "cancelled" => {
            // User-initiated cancellation — not an error from the UI's perspective.
            Err("cancelled".to_string())
        }
        Err(e) => {
            tracing::error!("Failed to receive file: {}", e);
            Err(format!("Failed to receive file: {}", e))
        }
    }
}

/// Where an export put the files: `display` is what history shows, `uri` is the
/// SAF tree Open reopens. MediaStore has no tree — its relative path is enough
/// to raise a folder intent.
#[cfg(target_os = "android")]
struct ExportDestination {
    display: String,
    uri: Option<String>,
}

/// What an Android export needs to know. `root` and `files_dir` differ only when
/// an auto-accepted transfer filed itself under a per-device subfolder: the copy
/// walks the root so that subfolder is recreated at the destination, while a
/// failed copy leaves the files sitting in `files_dir`.
#[cfg(target_os = "android")]
struct AndroidExport<'a> {
    root: &'a Path,
    files_dir: &'a Path,
    tree_uri: Option<&'a str>,
    tree_display_path: Option<&'a str>,
}

/// Move a finished receive out of staging, bracketed by a completion event.
///
/// `receive-completed` fires before this re-copies the bytes out, so the
/// success screen is up while the destination doesn't exist yet. Emitting on
/// the way out gives the UI one signal for "the files are where they belong".
///
/// Returns the human-readable destination, or `None` when the files stayed in
/// staging — then the path recorded at open is already the truthful one.
#[cfg(target_os = "android")]
fn finalize_android_receive(
    app_handle: &tauri::AppHandle,
    export: AndroidExport<'_>,
) -> Result<Option<ExportDestination>, String> {
    let result = export_android_receive(app_handle, export);
    let _ = app_handle.emit("receive-export-finished", ());
    result
}

#[cfg(target_os = "android")]
fn export_android_receive(
    app_handle: &tauri::AppHandle,
    export: AndroidExport<'_>,
) -> Result<Option<ExportDestination>, String> {
    use tauri_plugin_native_utils::{ExportToTreeArgs, NativeUtilsExt};

    let staging_dir = export.root;
    let tree_uri = export.tree_uri.map(str::trim).filter(|uri| !uri.is_empty());

    let Some(tree_uri) = tree_uri else {
        return finalize_android_media_store_receive(app_handle, staging_dir, export.files_dir);
    };

    let export_result = app_handle.native_utils().export_to_tree(ExportToTreeArgs {
        tree_uri: tree_uri.to_string(),
        source_dir: staging_dir.to_string_lossy().into_owned(),
    });

    match export_result {
        Ok(result) => {
            tracing::info!(
                exported = result.exported_count,
                conflicts = result.conflicts.len(),
                "Exported received files to SAF tree"
            );
            if let Err(e) = std::fs::remove_dir_all(staging_dir) {
                tracing::warn!(
                    "Failed to clean staging dir after SAF export ({}): {}",
                    staging_dir.display(),
                    e
                );
            }
            // Only the picker knows the tree URI's readable form; without it
            // there is nothing truthful to show.
            Ok(export
                .tree_display_path
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(|display| ExportDestination {
                    display: display.to_string(),
                    uri: Some(tree_uri.to_string()),
                }))
        }
        Err(e) => {
            tracing::warn!("SAF export failed, keeping app-private files: {e}");
            emit_receive_download_fallback(app_handle, export.files_dir, "saf");
            // Transfer itself succeeded — files remain in staging.
            Ok(None)
        }
    }
}

/// Export a finished receive into the public `Download/DashBeam` collection —
/// the zero-configuration path, no folder picked or permission prompted. Files
/// left in app-private staging can't be opened, so a failed export falls back
/// and tells the user where they ended up.
#[cfg(target_os = "android")]
fn finalize_android_media_store_receive(
    app_handle: &tauri::AppHandle,
    staging_dir: &Path,
    files_dir: &Path,
) -> Result<Option<ExportDestination>, String> {
    use tauri_plugin_native_utils::{
        ExportToMediaStoreArgs, NativeUtilsExt, MEDIA_STORE_UNSUPPORTED,
    };

    let export_result = app_handle
        .native_utils()
        .export_to_media_store(ExportToMediaStoreArgs {
            source_dir: staging_dir.to_string_lossy().into_owned(),
        });

    match export_result {
        Ok(result) => {
            tracing::info!(
                exported = result.exported_count,
                conflicts = result.conflicts.len(),
                path = %result.display_path,
                "Exported received files to the Downloads collection"
            );
            if let Err(e) = std::fs::remove_dir_all(staging_dir) {
                tracing::warn!(
                    "Failed to clean staging dir after MediaStore export ({}): {}",
                    staging_dir.display(),
                    e
                );
            }
            let payload = serde_json::json!({
                "path": result.display_path,
                "uris": result.uris,
            });
            let _ = app_handle.emit("receive-download-mediastore", payload);
            Ok(Some(ExportDestination {
                display: result.display_path,
                uri: None,
            }))
        }
        Err(e) => {
            let message = e.to_string();
            if message.contains(MEDIA_STORE_UNSUPPORTED) {
                tracing::info!("MediaStore unavailable on this device; keeping app-private files");
            } else {
                tracing::warn!("MediaStore export failed, keeping app-private files: {message}");
            }
            // Transfer itself succeeded — files remain in staging.
            emit_receive_download_fallback(app_handle, files_dir, "private");
            Ok(None)
        }
    }
}

#[cfg(target_os = "android")]
fn emit_receive_download_fallback(app_handle: &tauri::AppHandle, staging_dir: &Path, reason: &str) {
    let payload = serde_json::json!({
        "path": staging_dir.to_string_lossy(),
        "reason": reason,
    });
    let _ = app_handle.emit("receive-download-fallback", payload);
}

/// Android writes a receive into app-private staging and copies it out once the
/// transfer finishes. Both exporters recreate each file's path *relative to the
/// directory they are handed*, so the staging root is returned alongside the
/// write dir: exporting from the root is what lets a per-device subfolder
/// survive the copy instead of collapsing into the destination.
#[cfg(target_os = "android")]
fn resolve_android_receive_dirs(
    app_handle: &tauri::AppHandle,
    sub_folder: Option<&str>,
) -> Result<(PathBuf, PathBuf), String> {
    let root = android_staging_receive_dir(app_handle)?;
    let output_dir = match sub_folder {
        Some(name) => root.join(name),
        None => root.clone(),
    };
    Ok((output_dir, root))
}

#[cfg(not(target_os = "android"))]
fn resolve_receive_output_dir(
    app_handle: &tauri::AppHandle,
    output_path: String,
    sub_folder: Option<&str>,
) -> Result<PathBuf, String> {
    let base = PathBuf::from(output_path.trim());
    let base = if base.as_os_str().is_empty() {
        fallback_receive_dir(app_handle)?
    } else {
        base
    };
    // `ensure_dir_writable` calls `create_dir_all`, so the per-device folder
    // is created here on first use.
    let output_dir = match sub_folder {
        Some(name) => base.join(name),
        None => base,
    };

    match ensure_dir_writable(&output_dir) {
        Ok(()) => Ok(output_dir),
        Err(error) => {
            tracing::warn!(
                "Receive output dir not writable ({}): {}",
                output_dir.display(),
                error
            );
            Err("Selected download folder is not writable".to_string())
        }
    }
}

#[cfg(target_os = "android")]
fn android_staging_receive_dir(app_handle: &tauri::AppHandle) -> Result<PathBuf, String> {
    let transfer_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))?
        .join("downloads")
        .join(format!("recv-{transfer_id}"));
    ensure_dir_writable(&staging)
        .map_err(|e| format!("Failed to prepare staging download dir: {e}"))?;
    Ok(staging)
}

#[cfg(not(target_os = "android"))]
fn fallback_receive_dir(app_handle: &tauri::AppHandle) -> Result<PathBuf, String> {
    let fallback = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))?
        .join("downloads");
    ensure_dir_writable(&fallback)
        .map_err(|e| format!("Failed to prepare fallback download dir: {e}"))?;
    Ok(fallback)
}

fn ensure_dir_writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe_name = format!(
        ".sendme_write_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let probe_path = dir.join(probe_name);
    std::fs::write(&probe_path, b"probe")?;
    std::fs::remove_file(probe_path)?;
    Ok(())
}

/// Cancel the currently active receive, if any.
/// Partial data is preserved on disk so the transfer can be resumed.
#[tauri::command]
pub async fn cancel_receive(state: State<'_, AppStateMutex>) -> Result<(), String> {
    let mut app_state = state.lock().await;
    if let Some(tx) = app_state.current_receive_cancel.take() {
        // Sending () signals the download future to stop. If the receiver is
        // already gone (download finished first) this is a harmless no-op.
        let _ = tx.send(());
    }
    Ok(())
}

/// Get the current sharing status
#[tauri::command]
pub async fn get_sharing_status(state: State<'_, AppStateMutex>) -> Result<Option<String>, String> {
    let app_state = state.lock().await;
    Ok(app_state
        .current_share
        .as_ref()
        .map(|share| share.ticket.clone()))
}

/// Check if a path is a file or directory
#[tauri::command]
pub async fn check_path_type(path: String) -> Result<String, String> {
    let path = PathBuf::from(path);

    if !path.exists() {
        return Err("Path does not exist".to_string());
    }

    if path.is_dir() {
        Ok("directory".to_string())
    } else if path.is_file() {
        Ok("file".to_string())
    } else {
        Err("Path is neither a file nor a directory".to_string())
    }
}

#[tauri::command]
pub async fn get_paths_mime_types(paths: Vec<String>) -> Result<Vec<Option<String>>, String> {
    let result = paths
        .into_iter()
        .map(|path| {
            let path_buf = PathBuf::from(path);
            if path_buf.is_dir() {
                return Some("inode/directory".to_string());
            }
            if path_buf.is_file() {
                return Some(
                    mime_guess::from_path(path_buf)
                        .first_or_octet_stream()
                        .essence_str()
                        .to_string(),
                );
            }
            None
        })
        .collect();

    Ok(result)
}

/// Get the current transport status (whether bytes are actively being transferred)
#[tauri::command]
pub async fn get_transport_status(state: State<'_, AppStateMutex>) -> Result<bool, String> {
    let app_state = state.lock().await;
    Ok(app_state.is_transporting)
}

/// Check if there was a launch intent (file path passed via CLI)
/// Returns the path if present and clears it from state
#[tauri::command]
pub async fn check_launch_intent(
    state: State<'_, AppStateMutex>,
) -> Result<Option<String>, String> {
    let mut app_state = state.lock().await;
    Ok(app_state.launch_intent.take())
}

#[tauri::command]
pub async fn toggle_context_menu(
    enable: bool,
    #[allow(unused_variables)] allow_elevation: Option<bool>,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if enable {
            crate::platform::windows::context_menu::register_context_menu()
                .map_err(|e| e.to_string())
        } else {
            crate::platform::windows::context_menu::unregister_context_menu(
                allow_elevation.unwrap_or(true),
            )
            .map_err(|e| e.to_string())
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = enable;
        Ok(())
    }
}

/// True when running the Windows no-install ZIP layout (`.portable` marker).
#[tauri::command]
pub fn is_windows_portable() -> bool {
    crate::platform::windows::portable::is_portable()
}

/// State of the debug-logging toggle. `enabled` is persisted;
/// `active_this_session` is whether the file sink was installed at launch —
/// they disagree between toggling and restarting, which drives the UI hint.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugLoggingState {
    pub enabled: bool,
    pub active_this_session: bool,
    pub log_dir: Option<String>,
}

fn app_config_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map_err(|e| format!("Failed to resolve config dir: {e}"))
}

fn app_log_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_log_dir()
        .map_err(|e| format!("Failed to resolve log dir: {e}"))
}

#[tauri::command]
pub fn get_debug_logging(app: tauri::AppHandle) -> Result<DebugLoggingState, String> {
    let config_dir = app_config_dir(&app)?;
    Ok(DebugLoggingState {
        enabled: crate::logging::is_enabled(&config_dir),
        active_this_session: crate::logging::is_active(),
        log_dir: app_log_dir(&app)
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
    })
}

/// Takes effect next launch; the subscriber is never reconfigured while
/// running. Turning it off purges immediately, so "off" means the logs are gone
/// even if the app is never reopened.
#[tauri::command]
pub fn set_debug_logging(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let config_dir = app_config_dir(&app)?;
    crate::logging::set_enabled(&config_dir, enabled)
        .map_err(|e| format!("Failed to update debug logging: {e}"))?;

    if !enabled {
        if let Ok(log_dir) = app_log_dir(&app) {
            // Best-effort: the current session's file may still be open on
            // Windows, and startup pruning finishes the job.
            let _ = crate::logging::clear(&log_dir);
        }
    }

    Ok(())
}

#[tauri::command]
pub fn clear_debug_logs(app: tauri::AppHandle) -> Result<(), String> {
    let log_dir = app_log_dir(&app)?;
    crate::logging::clear(&log_dir).map_err(|e| format!("Failed to clear logs: {e}"))
}

/// Largest bundle we will write. Comfortably under GitHub's attachment limit.
const MAX_BUNDLE_BYTES: usize = 5 * 1024 * 1024;

/// Write a metadata header plus captured logs to `dest_path`. Assembled here
/// rather than in the frontend so a partly broken UI still produces a full file.
#[tauri::command]
pub async fn export_debug_bundle(
    app: tauri::AppHandle,
    dest_path: String,
    relay: Option<RelayConfigArg>,
    #[allow(unused_variables)] state: State<'_, AppStateMutex>,
) -> Result<(), String> {
    let log_dir = app_log_dir(&app)?;

    let mut out = String::new();
    out.push_str("DashBeam diagnostics\n");
    out.push_str("====================\n\n");
    out.push_str(&format!(
        "App version: {}\n",
        crate::version::get_app_version()
    ));
    // `std::env::consts::OS` gives "macos" with no version.
    out.push_str(&format!(
        "OS: {} {} ({})\n",
        tauri_plugin_os::platform(),
        tauri_plugin_os::version(),
        std::env::consts::ARCH
    ));
    out.push_str(&format!(
        "Debug logging active this session: {}\n",
        crate::logging::is_active()
    ));

    // Only the frontend knows which relay the user selected.
    match relay.as_ref() {
        Some(config) => {
            out.push_str(&format!("Relay mode: {}\n", config.mode));
            if !config.urls.iter().all(|u| u.trim().is_empty()) {
                out.push_str(&format!("Relay URLs: {}\n", config.urls.join(", ")));
            }
            if let Some(fallback) = config.fallback.as_ref() {
                out.push_str(&format!("Relay fallback: {fallback}\n"));
            }
            out.push_str(&format!(
                "Relay auth token configured: {}\n",
                config.auth_token.as_ref().is_some_and(|t| !t.is_empty())
            ));
        }
        None => out.push_str("Relay mode: unknown (not reported by the UI)\n"),
    }

    match engine_get_relay_status(relay).await {
        Ok(status) => out.push_str(&format!(
            "Relay reachable: {} (kind={}, url={}, fell_back_to_public={})\n",
            status.connected,
            status.kind,
            status.url.unwrap_or_else(|| "-".to_string()),
            status.fell_back_to_public
        )),
        Err(error) => out.push_str(&format!("Relay status check failed: {error}\n")),
    }

    for (label, key) in [
        ("Session type", "XDG_SESSION_TYPE"),
        ("Wayland display", "WAYLAND_DISPLAY"),
        ("AppImage", "APPIMAGE"),
        ("Flatpak", "FLATPAK_ID"),
    ] {
        if let Ok(value) = std::env::var(key) {
            out.push_str(&format!("{label}: {value}\n"));
        }
    }

    #[cfg(any(desktop, target_os = "android", target_os = "ios"))]
    {
        let guard = state.lock().await;
        match guard.node.as_ref() {
            Some(node) => {
                let info = node.device_info();
                out.push_str(&format!("Device name: {}\n", info.display_name));
                out.push_str(&format!("Device type: {}\n", info.device_type));
                out.push_str(&format!("Device OS: {}\n", info.os));
                out.push_str(&format!("Endpoint ID: {}\n", info.endpoint_id));
            }
            None => {
                out.push_str("Device node: unavailable\n");
                if let Some(error) = guard.node_init_error.as_ref() {
                    out.push_str(&format!("Device node error: {error}\n"));
                }
            }
        }
    }

    out.push_str("\n--- logs ---\n\n");
    let remaining = MAX_BUNDLE_BYTES.saturating_sub(out.len());
    match crate::logging::read_logs(&log_dir, remaining) {
        Ok(logs) if logs.is_empty() => {
            out.push_str("(no logs captured — enable debug mode and restart)\n");
        }
        Ok(logs) => out.push_str(&logs),
        Err(error) => out.push_str(&format!("(failed to read logs: {error})\n")),
    }

    write_bundle(&app, dest_path, out)
}

/// Android's save dialog runs `ACTION_CREATE_DOCUMENT`, which creates the file
/// and hands back a `content://` URI — not a path. Writing it with `std::fs`
/// fails and leaves the 0-byte document the dialog just created.
#[allow(unused_variables)]
fn write_bundle(app: &tauri::AppHandle, dest_path: String, contents: String) -> Result<(), String> {
    #[cfg(target_os = "android")]
    if dest_path.starts_with("content://") {
        use tauri_plugin_native_utils::{NativeUtilsExt, WriteTextToUriArgs};

        return app
            .native_utils()
            .write_text_to_uri(WriteTextToUriArgs {
                uri: dest_path,
                contents,
            })
            .map_err(|e| format!("Failed to write diagnostics: {e}"));
    }

    std::fs::write(&dest_path, contents).map_err(|e| format!("Failed to write {dest_path}: {e}"))
}

/// Android cannot use the desktop updater plugin (it is `#[cfg(desktop)]`, and
/// a Play build may not update itself), so this only reports a newer release
/// and the UI hands the user to the release page.
///
/// The fetch goes through the Android plugin rather than `reqwest`: rustls
/// verifies certificates via `rustls-platform-verifier`, which needs a JVM
/// handshake this app never performs.
#[cfg(target_os = "android")]
#[tauri::command]
pub async fn check_android_update(
    app: tauri::AppHandle,
) -> Result<Option<crate::android_update::AndroidUpdate>, String> {
    use tauri_plugin_native_utils::NativeUtilsExt;

    let body = app
        .native_utils()
        .fetch_update_manifest()
        .map_err(|e| format!("Could not reach the update server: {e}"))?;

    crate::android_update::parse_manifest(&body, &crate::version::get_app_version())
}

/// Helper function to calculate total size of a file or directory
fn get_total_size(path: &Path) -> Result<u64, String> {
    if path.is_file() {
        return std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(|e| format!("Failed to read metadata for {}: {e}", path.display()));
    }

    if path.is_dir() {
        let mut total_size = 0u64;
        for entry in walkdir::WalkDir::new(path) {
            let entry = entry.map_err(|e| format!("Failed to traverse {}: {e}", path.display()))?;
            if entry.file_type().is_file() {
                let metadata = entry.metadata().map_err(|e| {
                    format!(
                        "Failed to read metadata for {}: {e}",
                        entry.path().display()
                    )
                })?;
                total_size = total_size.saturating_add(metadata.len());
            }
        }
        return Ok(total_size);
    }

    Err(format!(
        "Path is neither a file nor a directory: {}",
        path.display()
    ))
}

fn dedup_name(name: &str, seen: &mut BTreeMap<String, usize>) -> String {
    match seen.get_mut(name) {
        Some(count) => {
            *count += 1;
            format!("{} ({})", name, count)
        }
        None => {
            seen.insert(name.to_string(), 1);
            name.to_string()
        }
    }
}

async fn collect_preview_items(paths: &[PathBuf]) -> Result<Vec<FilePreviewItem>, String> {
    let mut items = Vec::with_capacity(paths.len());
    let mut seen_names = BTreeMap::new();

    for path in paths {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("item")
            .to_string();
        let final_name = dedup_name(&file_name, &mut seen_names);
        let size = get_total_size(path)?;
        let mime_type = if path.is_dir() {
            Some("inode/directory".to_string())
        } else {
            Some(
                mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_string(),
            )
        };
        let thumbnail = if path.is_file() {
            generate_thumbnail(path).await
        } else {
            None
        };
        items.push(FilePreviewItem {
            file_name: final_name,
            size,
            thumbnail,
            mime_type,
        });
    }

    items.sort_by(|a, b| a.file_name.cmp(&b.file_name));

    Ok(items)
}

/// Verify connectivity to configured relay servers.
#[tauri::command]
pub async fn verify_relays(relay: RelayConfigArg) -> Result<VerifyRelaysResponse, String> {
    engine_verify_relays(relay).await
}

/// Verify reachability of a custom self-hosted discovery (pkarr) server.
#[tauri::command]
pub async fn verify_discovery(
    discovery: DiscoveryConfigArg,
) -> Result<VerifyDiscoveryResponse, String> {
    engine_verify_discovery(discovery).await
}

/// Reads `discoverability` out of the raw `settings.json` the frontend's
/// `useAppSettingStore` writes (zustand `persist` envelope under
/// `app_settings`). `None` when missing or malformed — callers default.
#[cfg(any(desktop, target_os = "android", target_os = "ios", test))]
fn parse_persisted_discoverability(raw: &str) -> Option<Discoverability> {
    let file: serde_json::Value = serde_json::from_str(raw).ok()?;
    let envelope: serde_json::Value =
        serde_json::from_str(file.get("app_settings")?.as_str()?).ok()?;
    serde_json::from_value(envelope.get("state")?.get("discoverability")?.clone()).ok()
}

/// iOS drops the raw multicast socket mDNS uses unless the app holds the
/// Apple-approved com.apple.developer.networking.multicast entitlement, so
/// Nearby stays off there whatever was persisted or requested. The frontend
/// hides it to match (`IS_NEARBY_CAPABLE`).
#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
fn effective_discoverability(setting: Discoverability) -> Discoverability {
    if cfg!(target_os = "ios") {
        Discoverability::Off
    } else {
        setting
    }
}

/// Reads the persisted discoverability choice before `NodeService::start`, so
/// an `Off` install never registers mDNS even briefly (`DeviceNodeSync`
/// re-applies it once the webview loads).
///
/// A raw file read, not `StoreExt::store`: loading the store Rust-side would
/// register it without the frontend's `LazyStore` options, and the plugin
/// reuses whichever instance registered first.
#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
fn load_persisted_discoverability(app_handle: &tauri::AppHandle) -> Discoverability {
    let Ok(data_dir) = app_handle.path().app_data_dir() else {
        return Discoverability::default();
    };
    let Ok(raw) = std::fs::read_to_string(data_dir.join("settings.json")) else {
        return Discoverability::default();
    };
    parse_persisted_discoverability(&raw).unwrap_or_default()
}

/// Reads `minimizeToTray` out of `settings.json` — same envelope and same
/// raw-read reasoning as `parse_persisted_discoverability`.
#[cfg(any(desktop, test))]
fn parse_persisted_minimize_to_tray(raw: &str) -> Option<bool> {
    let file: serde_json::Value = serde_json::from_str(raw).ok()?;
    let envelope: serde_json::Value =
        serde_json::from_str(file.get("app_settings")?.as_str()?).ok()?;
    envelope.get("state")?.get("minimizeToTray")?.as_bool()
}

/// The "keep running in the background" choice, read before the first window
/// close. Defaults to `true` — every existing install already closes to tray.
#[cfg(desktop)]
pub fn load_persisted_minimize_to_tray(app_handle: &tauri::AppHandle) -> bool {
    let Ok(data_dir) = app_handle.path().app_data_dir() else {
        return true;
    };
    let Ok(raw) = std::fs::read_to_string(data_dir.join("settings.json")) else {
        return true;
    };
    parse_persisted_minimize_to_tray(&raw).unwrap_or(true)
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
pub async fn init_node_service(app_handle: tauri::AppHandle) -> Result<(), String> {
    let data_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?;

    let (relay_mode, _) = resolve_relay_mode_with_fallback(None).await?;
    let relay_mode: iroh::endpoint::RelayMode = relay_mode.into();
    let discovery_mode = build_discovery_mode(None)?;
    let discoverability = effective_discoverability(load_persisted_discoverability(&app_handle));

    let emitter = Arc::new(TauriEventEmitter {
        app_handle: app_handle.clone(),
    });
    let boxed_handle: AppHandle = Some(emitter);
    let node = NodeService::start(
        &data_dir,
        relay_mode,
        discovery_mode,
        discoverability,
        boxed_handle,
    )
    .await
    .map_err(|e| format!("Failed to start device node: {e}"))?;
    let state = app_handle.state::<AppStateMutex>();
    {
        let mut guard = state.lock().await;
        guard.node = Some(Arc::new(node));
        guard.node_init_error = None;
    }

    // Scoped above so the lock is released first: `refresh` takes it again.
    #[cfg(target_os = "android")]
    {
        let state = state.inner().clone();
        crate::presence_service::refresh(&app_handle, &state).await;
    }

    Ok(())
}

#[derive(serde::Serialize)]
pub struct NodeStatusResponse {
    pub status: String,
    pub reason: Option<String>,
    /// When status is ready: whether the home relay / network path is warmed up.
    #[serde(default)]
    pub network_ready: bool,
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
fn node_status_from_state(guard: &crate::state::AppState) -> NodeStatusResponse {
    if let Some(node) = &guard.node {
        return NodeStatusResponse {
            status: "ready".to_string(),
            reason: None,
            network_ready: node.is_network_ready(),
        };
    }
    // Init still in flight: distinguish from a hard failure so the UI keeps waiting.
    if guard.node_init_error.is_none() {
        return NodeStatusResponse {
            status: "starting".to_string(),
            reason: None,
            network_ready: false,
        };
    }
    NodeStatusResponse {
        status: "unavailable".to_string(),
        reason: guard.node_init_error.clone(),
        network_ready: false,
    }
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn get_node_status(
    state: State<'_, AppStateMutex>,
) -> Result<NodeStatusResponse, String> {
    let guard = state.lock().await;
    let status = node_status_from_state(&guard);

    Ok(status)
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn reconfigure_node_relay(
    relay: Option<RelayConfigArg>,
    discovery: Option<DiscoveryConfigArg>,
    state: State<'_, AppStateMutex>,
) -> Result<(), String> {
    // Captured before `relay` is consumed, so the log can say what was asked for.
    let relay_mode_label = relay
        .as_ref()
        .map(|r| r.mode.clone())
        .unwrap_or_else(|| "default".to_string());
    let relay_urls_label = relay
        .as_ref()
        .map(|r| r.urls.join(", "))
        .unwrap_or_default();

    let (relay_mode, _) = resolve_relay_mode_with_fallback(relay).await?;
    let relay_mode: iroh::endpoint::RelayMode = relay_mode.into();
    let discovery_mode = build_discovery_mode(discovery)?;

    let node = {
        let guard = state.lock().await;
        if guard.current_share.is_some() || guard.is_share_starting {
            return Err(
                "Stop sharing before changing network settings for paired devices.".to_string(),
            );
        }
        guard
            .node
            .clone()
            .ok_or_else(|| "Device pairing is not available on this device.".to_string())?
    };

    // Without this the log shows the endpoint suddenly re-homing with no explanation.
    tracing::debug!(
        target: "dashbeam::_events::relay::reconfigure",
        mode = %relay_mode_label,
        urls = %relay_urls_label,
        "relay/discovery settings changed"
    );

    node.reconfigure_network(relay_mode, discovery_mode)
        .await
        .map_err(|error| {
            tracing::error!(
                target: "dashbeam::_events::relay::reconfigure_failed",
                mode = %relay_mode_label,
                %error,
            );
            format!("Failed to update device network settings: {error}")
        })?;

    Ok(())
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn get_device_info(state: State<'_, AppStateMutex>) -> Result<DeviceInfo, String> {
    let guard = state.lock().await;
    let node = guard.node.as_ref().ok_or_else(|| {
        guard
            .node_init_error
            .clone()
            .unwrap_or_else(|| "Device pairing is not available.".to_string())
    })?;
    let info = node.device_info();

    Ok(info)
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn set_device_display_name(
    display_name: String,
    state: State<'_, AppStateMutex>,
) -> Result<DeviceInfo, String> {
    let guard = state.lock().await;
    let node = require_node(&guard)?;
    let info = node
        .set_device_display_name(&display_name)
        .map_err(|e| e.to_string())?;

    Ok(info)
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn rename_paired_device(
    endpoint_id: String,
    display_name: String,
    state: State<'_, AppStateMutex>,
) -> Result<PairedDevice, String> {
    let guard = state.lock().await;
    let node = require_node(&guard)?;
    let device = node
        .rename_paired(&endpoint_id, &display_name)
        .map_err(|e| e.to_string())?;

    Ok(device)
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn trust_paired_device(
    endpoint_id: String,
    trust: bool,
    state: State<'_, AppStateMutex>,
) -> Result<PairedDeviceInfo, String> {
    let guard = state.lock().await;
    let node = require_node(&guard)?;
    let device = node
        .trust_paired(&endpoint_id, trust)
        .map_err(|e| e.to_string())?;

    Ok(device)
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
fn require_node(guard: &crate::state::AppState) -> Result<&NodeService, String> {
    guard.node.as_deref().ok_or_else(|| {
        guard
            .node_init_error
            .clone()
            .unwrap_or_else(|| "Device pairing is not available.".to_string())
    })
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
fn require_node_arc(guard: &crate::state::AppState) -> Result<Arc<NodeService>, String> {
    guard.node.clone().ok_or_else(|| {
        guard
            .node_init_error
            .clone()
            .unwrap_or_else(|| "Device pairing is not available.".to_string())
    })
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn get_pairing_ticket(state: State<'_, AppStateMutex>) -> Result<String, String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    node.pairing_ticket().map_err(|e| e.to_string())
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn start_pairing_host(
    ttl_secs: Option<u64>,
    state: State<'_, AppStateMutex>,
) -> Result<String, String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    let ticket = node
        .start_pairing_host(ttl_secs)
        .await
        .map_err(|e| e.to_string())?;

    Ok(ticket)
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn stop_pairing_host(state: State<'_, AppStateMutex>) -> Result<(), String> {
    let node = {
        let guard = state.lock().await;
        guard.node.clone()
    };
    if let Some(node) = node {
        node.stop_pairing_host().await;
    }

    Ok(())
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn join_pairing(ticket: String, state: State<'_, AppStateMutex>) -> Result<(), String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    node.join_pairing(&ticket)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn list_paired_devices(
    state: State<'_, AppStateMutex>,
) -> Result<Vec<PairedDeviceInfo>, String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    let devices = node.list_paired().map_err(|e| e.to_string())?;

    Ok(devices)
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn forget_paired_device(
    endpoint_id: String,
    state: State<'_, AppStateMutex>,
    #[allow(unused_variables)] app_handle: tauri::AppHandle,
) -> Result<(), String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    node.forget_paired(&endpoint_id)
        .await
        .map_err(|e| e.to_string())?;

    // A local forget emits no `device-unpaired` (only a remote one does), so
    // the listener in `lib.rs` would not see this — refresh explicitly.
    #[cfg(target_os = "android")]
    {
        let state = state.inner().clone();
        crate::presence_service::refresh(&app_handle, &state).await;
    }

    Ok(())
}

#[derive(serde::Serialize)]
pub struct InviteDelivered {
    pub delivered: bool,
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn invite_paired_device(
    endpoint_id: String,
    blob_ticket: String,
    file_count: u32,
    total_size: u64,
    state: State<'_, AppStateMutex>,
) -> Result<InviteDelivered, String> {
    let node = {
        let guard = state.lock().await;
        // The recipient is known here; `share-peer-connected` carries only an id.
        note_share_peer(&guard, name_peer(endpoint_id.clone(), &guard.node));
        require_node_arc(&guard)?
    };
    let delivered = node
        .invite_paired_device(&endpoint_id, &blob_ticket, file_count, total_size)
        .await
        .map_err(|e| e.to_string())?;

    Ok(InviteDelivered { delivered })
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn respond_paired_invite(
    endpoint_id: String,
    accepted: bool,
    state: State<'_, AppStateMutex>,
) -> Result<(), String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    node.respond_paired_invite(&endpoint_id, accepted)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn list_nearby(state: State<'_, AppStateMutex>) -> Result<Vec<NearbyDevice>, String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    Ok(node.list_nearby().await)
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn get_discoverability(
    state: State<'_, AppStateMutex>,
) -> Result<Discoverability, String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    Ok(node.discoverability().await)
}

/// The frontend seam is `setDiscoverability` in
/// `frontend/src/lib/pairing-api.ts` — its invoke payload key must match this
/// command's `setting` parameter name.
#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn set_discoverability(
    setting: Discoverability,
    state: State<'_, AppStateMutex>,
    #[allow(unused_variables)] app_handle: tauri::AppHandle,
) -> Result<(), String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    let setting = effective_discoverability(setting);
    node.set_discoverability(setting).await.map_err(|error| {
        tracing::error!(
            target: "dashbeam::_events::nearby::set_discoverability_failed",
            ?setting,
            %error,
        );
        format!("Failed to update discoverability: {error}")
    })?;

    // Turning discovery off can retire the background service, but only when
    // no paired device still needs presence held open.
    #[cfg(target_os = "android")]
    {
        let state = state.inner().clone();
        crate::presence_service::refresh(&app_handle, &state).await;
    }

    Ok(())
}

#[derive(serde::Serialize)]
pub struct NearbyStatusResponse {
    /// Why LAN discovery is unavailable (mDNS pump failed to start), or
    /// `None` when it's running or deliberately off. Queryable because the
    /// `nearby-unavailable` event can fire during node init, before the
    /// frontend has any listener registered.
    pub reason: Option<String>,
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn nearby_status(
    state: State<'_, AppStateMutex>,
) -> Result<NearbyStatusResponse, String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    Ok(NearbyStatusResponse {
        reason: node.nearby_unavailable_reason(),
    })
}

/// Delivers the caller's active share ticket to a Nearby device over the
/// control ALPN — same path as `invite_paired_device`. The receiver's
/// fingerprint confirmation is what promotes the peer to paired.
#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn invite_nearby_device(
    endpoint_id: String,
    blob_ticket: String,
    file_count: u32,
    total_size: u64,
    state: State<'_, AppStateMutex>,
) -> Result<InviteDelivered, String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };

    // A Nearby device's name lives only in the discovery cache, and
    // `list_nearby` is async — out of reach of the synchronous emitter path.
    let nearby_peer = node
        .list_nearby()
        .await
        .into_iter()
        .find(|d| d.endpoint_id.eq_ignore_ascii_case(&endpoint_id))
        .map(|d| TransferPeer {
            endpoint_id: d.endpoint_id,
            display_name: d.display_name,
            device_type: d.device_type,
        })
        .unwrap_or_else(|| TransferPeer {
            endpoint_id: endpoint_id.clone(),
            display_name: None,
            device_type: None,
        });
    note_share_peer(&*state.lock().await, nearby_peer);
    let delivered = node
        .invite_nearby_device(&endpoint_id, &blob_ticket, file_count, total_size)
        .await
        .map_err(|e| e.to_string())?;

    Ok(InviteDelivered { delivered })
}

/// Asks a Nearby device to pair (no file share). Receiver confirms on name /
/// device type; accept reuses `respond_nearby_invite`.
#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn request_nearby_pair(
    endpoint_id: String,
    state: State<'_, AppStateMutex>,
) -> Result<InviteDelivered, String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    let delivered = node
        .request_nearby_pair(&endpoint_id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(InviteDelivered { delivered })
}

#[cfg(any(desktop, target_os = "android", target_os = "ios"))]
#[tauri::command]
pub async fn respond_nearby_invite(
    endpoint_id: String,
    accept: bool,
    block: bool,
    state: State<'_, AppStateMutex>,
) -> Result<(), String> {
    let node = {
        let guard = state.lock().await;
        require_node_arc(&guard)?
    };
    let result = if accept {
        node.accept_nearby_invite(&endpoint_id).await
    } else {
        node.decline_nearby_invite(&endpoint_id, block).await
    };
    result.map_err(|e| e.to_string())
}

/// Show a desktop notification that stays as long as the OS allows. The Tauri
/// notification plugin exposes no timeout API and defaults to a couple of
/// seconds, so invite toasts go through notify-rust with `Timeout::Never`.
#[cfg(desktop)]
#[tauri::command]
pub fn show_system_notification(
    app: tauri::AppHandle,
    title: String,
    body: Option<String>,
    icon: Option<String>,
) -> Result<(), String> {
    let mut notification = notify_rust::Notification::new();
    let app_name = app
        .config()
        .product_name
        .clone()
        .unwrap_or_else(|| "DashBeam".into());
    notification.appname(&app_name);
    notification.summary(&title);
    if let Some(body) = body.as_deref() {
        notification.body(body);
    }
    if let Some(icon) = icon.as_deref() {
        notification.icon(icon);
    } else {
        notification.auto_icon();
    }
    notification.timeout(notify_rust::Timeout::Never);

    #[cfg(windows)]
    {
        let exe = tauri::utils::platform::current_exe().map_err(|e| e.to_string())?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| "failed to get exe directory".to_string())?;
        let curr_dir = exe_dir.display().to_string();
        let sep = std::path::MAIN_SEPARATOR;
        // AppUserModelID only when installed — matching tauri-plugin-notification.
        if !(curr_dir.ends_with(&format!("{sep}target{sep}debug"))
            || curr_dir.ends_with(&format!("{sep}target{sep}release")))
        {
            notification.app_id(&app.config().identifier);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let identifier = app.config().identifier.clone();
        let _ = notify_rust::set_application(if tauri::is_dev() {
            "com.apple.Terminal"
        } else {
            &identifier
        });
    }

    notification.show().map_err(|e| e.to_string())?;
    Ok(())
}

/// Mirror the frontend's "keep running in the background" switch into the
/// process-wide flag the window-close handler reads.
#[cfg(desktop)]
#[tauri::command]
pub fn set_background_on_close(enabled: bool) {
    crate::tray::set_background_on_close(enabled);
}

/// Push translated tray strings from the frontend. Best-effort: a missing
/// tray (build failed, or not yet created) is a no-op.
#[cfg(desktop)]
#[tauri::command]
pub fn set_tray_labels(app_handle: tauri::AppHandle, labels: crate::tray::TrayLabels) {
    crate::tray::apply_labels(&app_handle, labels);
}

/// Whether the OS currently launches DashBeam at sign-in. The OS is the
/// source of truth — a user who removed the login item outside the app must
/// see the toggle turn itself off.
/// `null` means the platform cannot be asked (Flatpak) — the caller keeps
/// its cached value rather than prompting the user.
#[cfg(desktop)]
#[tauri::command]
pub fn autostart_is_enabled(app_handle: tauri::AppHandle) -> Result<Option<bool>, String> {
    crate::autostart::is_enabled(&app_handle)
}

/// Request an autostart change. Returns the state the OS ended up in, which may
/// differ from `enabled` when the platform or the user refuses.
///
/// `async` on purpose: the Flatpak path blocks on a portal consent dialog, and
/// Tauri runs synchronous commands on the main thread.
#[cfg(desktop)]
#[tauri::command]
pub async fn autostart_set(app_handle: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    crate::autostart::set(&app_handle, enabled).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::{start_share, RelayModeOption};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_file(name_prefix: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        engine::storage::temp_dir().join(format!(
            "{}-{}-{}.txt",
            name_prefix,
            std::process::id(),
            ts
        ))
    }

    #[test]
    fn a_single_received_file_opens_the_file_itself() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        fs::write(dir.path().join("photo.jpg"), b"x").expect("write");

        assert_eq!(
            resolve_open_target(&dir.path().to_string_lossy(), &["photo.jpg".to_string()],),
            Some(dir.path().join("photo.jpg"))
        );
    }

    #[test]
    fn a_received_folder_opens_the_folder_not_the_download_dir() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        fs::create_dir(dir.path().join("Photos")).expect("mkdir");
        fs::write(dir.path().join("Photos/a.jpg"), b"x").expect("write");
        fs::write(dir.path().join("Photos/b.jpg"), b"x").expect("write");

        assert_eq!(
            resolve_open_target(
                &dir.path().to_string_lossy(),
                &["Photos/a.jpg".to_string(), "Photos/b.jpg".to_string()],
            ),
            Some(dir.path().join("Photos")),
            "the transfer's own folder is a better answer than everything around it"
        );
    }

    #[test]
    fn loose_files_open_the_directory_that_holds_them() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        fs::write(dir.path().join("a.txt"), b"x").expect("write");
        fs::write(dir.path().join("b.txt"), b"x").expect("write");

        assert_eq!(
            resolve_open_target(
                &dir.path().to_string_lossy(),
                &["a.txt".to_string(), "b.txt".to_string()],
            ),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn a_file_deleted_after_the_transfer_has_nothing_to_open() {
        let dir = tempfile::TempDir::new().expect("tempdir");

        assert_eq!(
            resolve_open_target(&dir.path().to_string_lossy(), &["gone.jpg".to_string()],),
            None,
            "the folder surviving is not the same as the files surviving"
        );
    }

    #[test]
    fn a_partly_deleted_transfer_still_opens_what_is_left() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        fs::write(dir.path().join("kept.txt"), b"x").expect("write");

        assert_eq!(
            resolve_open_target(
                &dir.path().to_string_lossy(),
                &["kept.txt".to_string(), "gone.txt".to_string()],
            ),
            Some(dir.path().join("kept.txt"))
        );
    }

    #[test]
    fn a_row_without_file_names_falls_back_to_its_folder() {
        let dir = tempfile::TempDir::new().expect("tempdir");

        assert_eq!(
            resolve_open_target(&dir.path().to_string_lossy(), &[]),
            Some(dir.path().to_path_buf())
        );
    }

    fn partial_store_path(root: &Path, hash: &str) -> PathBuf {
        root.join(format!("{}{hash}", engine::storage::RECV_DIR_PREFIX))
    }

    #[test]
    fn a_partial_store_the_launch_sweep_removed_has_nothing_to_free() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let gone = partial_store_path(temp.path(), &"a".repeat(64));

        let stats = stat_partial_store(Some(&gone.to_string_lossy()), temp.path());

        assert!(!stats.exists, "a store that is gone cannot be freed");
        assert_eq!(stats.size_bytes, 0);
    }

    #[test]
    fn an_empty_partial_store_has_nothing_to_free() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let empty = partial_store_path(temp.path(), &"b".repeat(64));
        fs::create_dir_all(&empty).expect("mkdir");

        let stats = stat_partial_store(Some(&empty.to_string_lossy()), temp.path());

        assert!(!stats.exists, "0 bytes is never worth offering to free");
    }

    #[test]
    fn a_partial_store_holding_bytes_reports_what_it_would_free() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let store = partial_store_path(temp.path(), &"c".repeat(64));
        fs::create_dir_all(store.join("data")).expect("mkdir");
        fs::write(store.join("data").join("blob"), b"1234").expect("write");

        let stats = stat_partial_store(Some(&store.to_string_lossy()), temp.path());

        assert!(stats.exists);
        assert_eq!(stats.size_bytes, 4);
    }

    #[test]
    fn a_name_that_escapes_the_destination_is_not_followed() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let outside = dir.path().join("outside");
        let dest = dir.path().join("dest");
        fs::create_dir_all(&outside).expect("mkdir");
        fs::create_dir_all(&dest).expect("mkdir");
        fs::write(outside.join("secret.txt"), b"x").expect("write");
        fs::write(dest.join("kept.txt"), b"x").expect("write");

        assert_eq!(
            resolve_open_target(
                &dest.to_string_lossy(),
                &["kept.txt".to_string(), "../outside/secret.txt".to_string()],
            ),
            Some(dest.join("kept.txt")),
            "only the name that stays under the destination counts"
        );
    }

    #[test]
    fn a_destination_that_is_gone_opens_nothing() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let missing = dir.path().join("removed");

        assert_eq!(resolve_open_target(&missing.to_string_lossy(), &[]), None);
        assert_eq!(resolve_open_target("", &["a.txt".to_string()]), None);
    }

    /// Locks the seam with the frontend's `useAppSettingStore`, which writes
    /// this `settings.json` layout.
    #[test]
    fn parse_persisted_discoverability_reads_the_zustand_envelope() {
        let envelope = serde_json::json!({ "state": { "discoverability": "off", "darkMode": true }, "version": 0 });
        let file = serde_json::json!({ "app_settings": envelope.to_string() }).to_string();
        assert_eq!(
            parse_persisted_discoverability(&file),
            Some(Discoverability::Off)
        );

        let envelope = serde_json::json!({ "state": { "discoverability": "paired-only" } });
        let file = serde_json::json!({ "app_settings": envelope.to_string() }).to_string();
        assert_eq!(
            parse_persisted_discoverability(&file),
            Some(Discoverability::PairedOnly)
        );
    }

    #[test]
    fn parse_persisted_discoverability_tolerates_missing_or_malformed_data() {
        assert_eq!(parse_persisted_discoverability("not json"), None);
        assert_eq!(parse_persisted_discoverability("{}"), None);
        let file = serde_json::json!({ "app_settings": "{\"state\":{}}" }).to_string();
        assert_eq!(parse_persisted_discoverability(&file), None);
        let envelope = serde_json::json!({ "state": { "discoverability": "bogus" } });
        let file = serde_json::json!({ "app_settings": envelope.to_string() }).to_string();
        assert_eq!(parse_persisted_discoverability(&file), None);
    }

    #[test]
    fn parse_persisted_minimize_to_tray_reads_the_zustand_envelope() {
        let envelope = serde_json::json!({ "state": { "minimizeToTray": false, "darkMode": true }, "version": 0 });
        let file = serde_json::json!({ "app_settings": envelope.to_string() }).to_string();
        assert_eq!(parse_persisted_minimize_to_tray(&file), Some(false));

        let envelope = serde_json::json!({ "state": { "minimizeToTray": true } });
        let file = serde_json::json!({ "app_settings": envelope.to_string() }).to_string();
        assert_eq!(parse_persisted_minimize_to_tray(&file), Some(true));
    }

    #[test]
    fn parse_persisted_minimize_to_tray_tolerates_missing_or_malformed_data() {
        assert_eq!(parse_persisted_minimize_to_tray("not json"), None);
        assert_eq!(parse_persisted_minimize_to_tray("{}"), None);
        let file = serde_json::json!({ "app_settings": "{\"state\":{}}" }).to_string();
        assert_eq!(parse_persisted_minimize_to_tray(&file), None);
        // Wrong type must not panic or coerce.
        let envelope = serde_json::json!({ "state": { "minimizeToTray": "yes" } });
        let file = serde_json::json!({ "app_settings": envelope.to_string() }).to_string();
        assert_eq!(parse_persisted_minimize_to_tray(&file), None);
    }

    #[tokio::test]
    async fn fetch_ticket_metadata_command_e2e() {
        let temp_path = unique_temp_file("sendme-tauri-meta");
        fs::write(&temp_path, b"tauri metadata preview test payload")
            .expect("should create temp payload file");

        let expected_metadata = FileMetadata {
            file_name: "preview-source.txt".to_string(),
            item_count: 1,
            size: 123,
            thumbnail: Some("data:image/jpeg;base64,ZmFrZS10aHVtYg==".to_string()),
            mime_type: Some("text/plain".to_string()),
            items: None,
        };

        let options = SendOptions {
            relay_mode: RelayModeOption::Default,
            discovery_mode: Default::default(),
            ticket_type: AddrInfoOptions::RelayAndAddresses,
            magic_ipv4_addr: None,
            magic_ipv6_addr: None,
        };

        let share = start_share(
            temp_path.clone(),
            options,
            None,
            Some(expected_metadata.clone()),
        )
        .await
        .expect("start_share should succeed");

        let fetched = fetch_ticket_metadata(share.ticket.clone(), None, None)
            .await
            .expect("fetch_ticket_metadata command should succeed");

        assert_eq!(fetched.file_name, expected_metadata.file_name);
        assert_eq!(fetched.size, expected_metadata.size);
        assert_eq!(fetched.thumbnail, expected_metadata.thumbnail);
        assert_eq!(fetched.mime_type, expected_metadata.mime_type);

        drop(share);
        let _ = fs::remove_file(temp_path);
    }
}

// ---------------------------------------------------------------------------
// Transfer history
// ---------------------------------------------------------------------------

/// Live report on a record's partial-receive store.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferTempData {
    pub exists: bool,
    pub size_bytes: u64,
}

#[tauri::command]
pub async fn list_transfer_history(
    history: State<'_, Arc<TransferHistoryStore>>,
) -> Result<Vec<TransferRecord>, String> {
    history.list().map_err(|e| e.to_string())
}

/// Removes one row and the partial store it pointed at — the row is the only
/// pointer to that disk space, so dropping it alone strands the bytes.
#[tauri::command]
pub async fn delete_transfer_record(
    id: String,
    history: State<'_, Arc<TransferHistoryStore>>,
) -> Result<(), String> {
    let removed = history.delete(&id).map_err(|e| e.to_string())?;
    if let Some(record) = removed {
        engine::reclaim_partial(&record, &engine::storage::temp_dir());
    }
    Ok(())
}

/// Where a row's files are now, or `None` when nothing it recorded is still
/// there. Resolved against the filesystem rather than trusted from the record:
/// a destination can be emptied, moved or deleted long after the transfer.
///
/// One surviving file is opened directly; several are shown in the deepest
/// folder that contains them, so a received directory opens itself instead of
/// everything around it.
fn resolve_open_target(save_path: &str, file_names: &[String]) -> Option<PathBuf> {
    let base = PathBuf::from(save_path.trim());
    if base.as_os_str().is_empty() {
        return None;
    }

    let existing: Vec<PathBuf> = file_names
        .iter()
        // A recorded name is a relative path under the destination. Anything
        // absolute or climbing out of it did not come from this transfer, and
        // would drag the common ancestor below `base`.
        .filter(|name| {
            let name = Path::new(name.as_str());
            name.is_relative()
                && name
                    .components()
                    .all(|part| !matches!(part, std::path::Component::ParentDir))
        })
        .map(|name| base.join(name))
        .filter(|path| path.exists())
        .collect();

    match existing.as_slice() {
        // Nothing was recorded to check, so the folder is the whole answer.
        [] if file_names.is_empty() => base.exists().then_some(base),
        [] => None,
        [only] => Some(only.clone()),
        [first, rest @ ..] => {
            let mut common = first.parent()?.to_path_buf();
            for path in rest {
                let Some(parent) = path.parent() else {
                    return Some(base);
                };
                while !parent.starts_with(&common) {
                    match common.parent() {
                        Some(up) => common = up.to_path_buf(),
                        None => return Some(base),
                    }
                }
            }
            Some(common)
        }
    }
}

/// Resolves what a history row's "Open" should reveal. `None` means the files
/// are gone — the UI says so rather than opening an empty folder.
#[tauri::command]
pub async fn transfer_open_target(
    id: String,
    history: State<'_, Arc<TransferHistoryStore>>,
) -> Result<Option<String>, String> {
    let record = history
        .list()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|record| record.id == id);

    let Some(record) = record else {
        return Ok(None);
    };
    let Some(save_path) = record.save_path.as_deref() else {
        return Ok(None);
    };

    Ok(resolve_open_target(save_path, &record.file_names)
        .map(|path| path.to_string_lossy().to_string()))
}

#[tauri::command]
pub async fn clear_transfer_history(
    history: State<'_, Arc<TransferHistoryStore>>,
) -> Result<(), String> {
    let removed = history.clear().map_err(|e| e.to_string())?;
    let temp_dir = engine::storage::temp_dir();
    for record in &removed {
        engine::reclaim_partial(record, &temp_dir);
    }
    Ok(())
}

/// Walks the record's partial store rather than trusting the record: the
/// directory can be removed by the OS, by the launch sweep, or by hand. Only a
/// store still on disk with bytes in it is worth offering to free.
fn stat_partial_store(raw_path: Option<&str>, temp_dir: &Path) -> TransferTempData {
    let nothing = TransferTempData {
        exists: false,
        size_bytes: 0,
    };
    let Some(path) = raw_path.map(PathBuf::from) else {
        return nothing;
    };
    if !path.is_dir() || !engine::is_reclaimable_partial(&path, temp_dir) {
        return nothing;
    }
    let size_bytes = get_total_size(&path).unwrap_or(0);

    TransferTempData {
        exists: size_bytes > 0,
        size_bytes,
    }
}

#[tauri::command]
pub async fn get_transfer_temp_data(
    id: String,
    history: State<'_, Arc<TransferHistoryStore>>,
) -> Result<TransferTempData, String> {
    let raw_path = history
        .list()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|r| r.id == id)
        .and_then(|r| r.resumable_store_path);

    tokio::task::spawn_blocking(move || {
        stat_partial_store(raw_path.as_deref(), &engine::storage::temp_dir())
    })
    .await
    .map_err(|e| format!("Task join error: {}", e))
}

/// Frees a record's partial store without removing the row.
#[tauri::command]
pub async fn clear_transfer_temp_data(
    id: String,
    history: State<'_, Arc<TransferHistoryStore>>,
    state: State<'_, AppStateMutex>,
) -> Result<(), String> {
    let record = history
        .list()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| "Transfer not found".to_string())?;

    // Deleting the store a running download is writing into would corrupt it.
    if let Some(active) = state.lock().await.current_receive_hash.as_deref() {
        if record.blob_hash.as_deref() == Some(active) {
            return Err("That transfer is downloading right now.".to_string());
        }
    }

    engine::reclaim_partial(&record, &engine::storage::temp_dir());
    history
        .update(&id, |r| r.resumable_store_path = None)
        .map_err(|e| e.to_string())?;
    Ok(())
}
