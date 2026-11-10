use protocol::{
    download_to_store, get_or_create_secret, AppHandle, DiscoveryModeOption, ReceiveOptions,
};
use iroh::endpoint::presets;
use iroh::{
    address_lookup::{dns::DnsAddressLookup, pkarr::PkarrResolver},
    Endpoint,
};
use iroh_blobs::api::blobs::ExportMode;
use iroh_blobs::ticket::BlobTicket;
use std::str::FromStr;
use tokio::select;
use crate::export::export_to_directory;
use crate::storage;
use crate::types::ReceiveResult;

fn emit_event_with_payload(app_handle: &AppHandle, event_name: &str, payload: &str) {
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event_with_payload(event_name, payload) {
            tracing::warn!("Failed to emit event {} with payload: {}", event_name, e);
        }
    }
}

pub async fn download(
    ticket_str: String,
    options: ReceiveOptions,
    app_handle: AppHandle,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<ReceiveResult> {
    let ticket = BlobTicket::from_str(&ticket_str)?;
    let addr = ticket.addr().clone();
    let secret_key = get_or_create_secret()?;

    let custom_infra =
        protocol::uses_custom_infra(&options.discovery_mode, &options.relay_mode);
    let mut builder =
        protocol::with_system_ca_if_custom(Endpoint::builder(presets::Minimal), custom_infra)
            .alpns(vec![])
            .secret_key(secret_key)
            .relay_mode(options.relay_mode.clone().into());

    if ticket.addr().relay_urls().count() == 0 && ticket.addr().ip_addrs().count() == 0 {
        builder = match &options.discovery_mode {
            DiscoveryModeOption::Custom {
                pkarr_relay_url,
                dns_origin,
            } => {
                let mut builder =
                    builder.address_lookup(PkarrResolver::builder(pkarr_relay_url.clone()));
                if let Some(origin) = dns_origin {
                    builder = builder.address_lookup(DnsAddressLookup::builder(origin.clone()));
                }
                builder
            }
            DiscoveryModeOption::Default => {
                builder.address_lookup(DnsAddressLookup::n0_dns())
            }
        };
    }
    if let Some(addr) = options.magic_ipv4_addr {
        builder = builder.bind_addr(addr)?;
    }
    if let Some(addr) = options.magic_ipv6_addr {
        builder = builder.bind_addr(addr)?;
    }

    let endpoint = builder.bind().await?;
    let output_dir = options
        .output_dir
        .clone()
        .unwrap_or_else(|| dirs::download_dir().unwrap_or_else(|| std::env::current_dir().unwrap()));
    let recv_store =
        storage::create_recv_store(&ticket.hash().to_hex().to_string(), &output_dir).await?;
    // Same volume as the destination: move the blobs into place. From the temp
    // dir the volumes may differ, so copy as before.
    let export_mode = if recv_store.in_destination {
        ExportMode::TryReference
    } else {
        ExportMode::Copy
    };
    let db = recv_store.store;
    let mut cleanup_guard = storage::recv_cleanup_guard(recv_store.path);
    let db2 = db.clone();

    let transfer = async {
        let downloaded =
            download_to_store(ticket, addr, &endpoint, db.as_ref(), &app_handle).await?;

        let export_start = std::time::Instant::now();
        let conflicts = export_to_directory(&db, downloaded.collection, &output_dir, export_mode).await?;
        let export_duration_ms =
            protocol::duration_ms(export_start.elapsed().as_secs_f64());

        if !conflicts.is_empty() {
            let payload = serde_json::to_string(&conflicts).unwrap_or_else(|_| "[]".to_string());
            emit_event_with_payload(&app_handle, "receive-conflicts", &payload);
        }

        // Writing the files out to disk is a separate cost from the transfer;
        // report them apart so both ends can compare like with like.
        // `outputDir` is where the bytes actually landed. The UI cannot re-derive
        // it once an auto-accepted transfer files itself under a per-device
        // subfolder, so "Open" would otherwise reveal the wrong directory.
        let completion = serde_json::json!({
            "durationMs": downloaded.download_duration_ms,
            "exportMs": export_duration_ms,
            "bytes": downloaded.payload_size,
            "outputDir": output_dir.to_string_lossy(),
        });
        emit_event_with_payload(&app_handle, "receive-completed", &completion.to_string());

        anyhow::Ok((
            downloaded.total_files,
            downloaded.payload_size,
            downloaded.stats,
            conflicts.len(),
        ))
    };

    let (total_files, payload_size, _stats, conflict_count) = match select! {
        result = transfer => result,
        _ = cancel_rx => {
            tracing::info!("Download cancelled by user — preserving partial store for resume");
            cleanup_guard.disarm();
            db2.shutdown().await?;
            endpoint.close().await;
            anyhow::bail!("cancelled");
        }
    } {
        Ok(values) => {
            endpoint.close().await;
            // Release the store's files before the guard deletes the directory;
            // Windows cannot remove files that are still open.
            if let Err(e) = db2.shutdown().await {
                tracing::warn!("recv store did not shut down cleanly: {e}");
            }
            values
        }
        Err(e) => {
            tracing::error!("Download operation failed: {e}");
            endpoint.close().await;
            cleanup_guard.disarm();
            db2.shutdown().await?;
            anyhow::bail!("error: {e}");
        }
    };

    let message = if conflict_count > 0 {
        format!(
            "Downloaded {} files, {} bytes ({} name conflicts auto-resolved)",
            total_files, payload_size, conflict_count
        )
    } else {
        format!("Downloaded {} files, {} bytes", total_files, payload_size)
    };

    Ok(ReceiveResult {
        message,
        file_path: output_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send::start_share;
    use protocol::{
        AddrInfoOptions, FileMetadata, RelayModeOption, SendOptions,
    };
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_fetch_metadata_e2e() {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "metadata e2e test content").unwrap();
        let temp_path = temp_file.path().to_path_buf();

        let expected_metadata = FileMetadata {
            file_name: "test_e2e_file.txt".into(),
            item_count: 1,
            size: 25,
            thumbnail: Some("data:image/jpeg;base64,e2e_test_thumbnail=".into()),
            mime_type: Some("text/plain".into()),
            items: None,
        };

        let send_opts = SendOptions {
            relay_mode: RelayModeOption::Default,
            discovery_mode: Default::default(),
            ticket_type: AddrInfoOptions::RelayAndAddresses,
            magic_ipv4_addr: None,
            magic_ipv6_addr: None,
        };

        let result = start_share(temp_path, send_opts, None, Some(expected_metadata.clone()))
            .await
            .expect("Failed to start share");

        let recv_opts = ReceiveOptions {
            output_dir: None,
            relay_mode: RelayModeOption::Default,
            discovery_mode: Default::default(),
            magic_ipv4_addr: None,
            magic_ipv6_addr: None,
        };

        let fetched = protocol::fetch_metadata(result.ticket, recv_opts)
            .await
            .expect("Failed to fetch metadata from node");

        assert_eq!(fetched.file_name, expected_metadata.file_name);
        assert_eq!(fetched.size, expected_metadata.size);
        assert_eq!(fetched.thumbnail, expected_metadata.thumbnail);
        assert_eq!(fetched.mime_type, expected_metadata.mime_type);
    }
}
