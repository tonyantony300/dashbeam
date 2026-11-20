use crate::types::{get_or_create_secret, DiscoveryModeOption};
use serde::{Deserialize, Serialize};

#[cfg(not(target_arch = "wasm32"))]
use crate::time_compat::{timeout, Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
pub struct DiscoveryConfigArg {
    pub mode: String,
    pub pkarr_relay_url: Option<String>,
    /// Optional DNS origin for real-DNS resolve (`DnsAddressLookup`), e.g. `example.com`.
    /// Empty / omitted keeps HTTPS pkarr resolve only.
    #[serde(default)]
    pub dns_origin: Option<String>,
}

const MAX_DISCOVERY_URL_LENGTH: usize = 2048;
const MAX_DNS_ORIGIN_LENGTH: usize = 253;

#[cfg(not(target_arch = "wasm32"))]
const DISCOVERY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

fn has_disallowed_discovery_text_char(value: &str) -> bool {
    value
        .chars()
        .any(|char| char.is_control() || char.is_whitespace())
}

fn is_loopback_discovery_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|addr| addr.is_loopback())
        .unwrap_or(false)
}

/// Parse and validate a self-hosted pkarr relay URL (e.g. `https://dns.example.com/pkarr`).
///
/// Mirrors the relay URL validation: HTTPS is required except for loopback hosts,
/// and embedded credentials / whitespace are rejected.
pub fn parse_pkarr_relay_url(url: &str) -> Result<url::Url, String> {
    if url.is_empty() {
        return Err("Discovery (pkarr) URL must not be empty".to_string());
    }
    if url.len() > MAX_DISCOVERY_URL_LENGTH {
        return Err("Discovery (pkarr) URL is too long".to_string());
    }
    if has_disallowed_discovery_text_char(url) {
        return Err(
            "Discovery (pkarr) URL must not contain whitespace or control characters".to_string(),
        );
    }

    let parsed = url::Url::parse(url).map_err(|_| "Invalid discovery (pkarr) URL".to_string())?;

    if parsed.username() != "" || parsed.password().is_some() {
        return Err("Discovery (pkarr) URL must not include a username or password".to_string());
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "Discovery (pkarr) URL must include a host".to_string())?;

    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if is_loopback_discovery_host(host) => Ok(parsed),
        "http" => {
            Err("Plain HTTP discovery URLs are only allowed for loopback hosts".to_string())
        }
        _ => Err("Discovery (pkarr) URL scheme must be https or loopback http".to_string()),
    }
}

fn is_valid_dns_origin_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    let bytes = label.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// Validate an optional DNS origin domain for `DnsAddressLookup` (e.g. `example.com`).
///
/// Empty / whitespace-only input becomes `None` (HTTPS-only resolve). Rejects
/// URLs, paths, ports, and other non-hostname forms.
pub fn parse_dns_origin(origin: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = origin.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    if raw.len() > MAX_DNS_ORIGIN_LENGTH {
        return Err("DNS origin is too long".to_string());
    }
    if has_disallowed_discovery_text_char(raw) {
        return Err("DNS origin must not contain whitespace or control characters".to_string());
    }
    if raw.contains("://") || raw.contains('/') || raw.contains('@') || raw.contains(':') {
        return Err(
            "DNS origin must be a hostname (e.g. example.com), not a URL".to_string(),
        );
    }

    let without_dot = raw.trim_end_matches('.');
    if without_dot.is_empty() {
        return Err("DNS origin must not be empty".to_string());
    }

    // Allow a single bare hostname or dotted domain; reject lone ".".
    if without_dot == "." {
        return Err("DNS origin must be a hostname (e.g. example.com)".to_string());
    }

    if !without_dot
        .split('.')
        .all(is_valid_dns_origin_label)
    {
        return Err("DNS origin contains an invalid hostname label".to_string());
    }

    Ok(Some(without_dot.to_ascii_lowercase()))
}

pub fn build_discovery_mode(arg: Option<DiscoveryConfigArg>) -> Result<DiscoveryModeOption, String> {
    match arg {
        None => Ok(DiscoveryModeOption::Default),
        Some(arg) => match arg.mode.as_str() {
            "default" => Ok(DiscoveryModeOption::Default),
            "custom" => {
                let url = arg
                    .pkarr_relay_url
                    .as_deref()
                    .ok_or_else(|| "A pkarr relay URL is required for custom discovery".to_string())?;
                let pkarr_relay_url = parse_pkarr_relay_url(url)?;
                let dns_origin = parse_dns_origin(arg.dns_origin.as_deref())?;
                Ok(DiscoveryModeOption::Custom {
                    pkarr_relay_url,
                    dns_origin,
                })
            }
            other => Err(format!("Invalid discovery mode: {other}")),
        },
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyDiscoveryResponse {
    pub url: Option<String>,
    pub latency_ms: u64,
}

/// Checks a custom discovery server end to end: publishes a throwaway record
/// (PUT) and reads it back (GET) over HTTPS. Talks only to that server — no
/// relay, no n0 infra.
///
/// Does not validate NS delegation / port 53 (`dns_origin`).
#[cfg(not(target_arch = "wasm32"))]
pub async fn verify_discovery(
    arg: DiscoveryConfigArg,
) -> Result<VerifyDiscoveryResponse, String> {
    verify_discovery_within(arg, DISCOVERY_PROBE_TIMEOUT).await
}

#[cfg(not(target_arch = "wasm32"))]
async fn verify_discovery_within(
    arg: DiscoveryConfigArg,
    deadline: Duration,
) -> Result<VerifyDiscoveryResponse, String> {
    use iroh::address_lookup::pkarr::{PkarrRelayClient, DEFAULT_PKARR_TTL};
    use iroh::address_lookup::{EndpointInfo, UserData};

    let mode = build_discovery_mode(Some(arg))?;
    let DiscoveryModeOption::Custom {
        pkarr_relay_url, ..
    } = mode
    else {
        return Err("Discovery verification requires custom discovery mode".to_string());
    };

    let tls = crate::tls_config::custom_infra_client_config()
        .map_err(|e| format!("Failed to set up TLS: {e}"))?;
    let client = PkarrRelayClient::new(pkarr_relay_url.clone(), tls, iroh::dns::DnsResolver::new());

    // Fresh key per probe: the record is never looked up by anyone else. The
    // user-data tag tells a server operator what these records are.
    let secret_key = get_or_create_secret().map_err(|e| e.to_string())?;
    let probe_tag: UserData = "dashbeam-discovery-check"
        .parse()
        .map_err(|e| format!("Failed to build probe record: {e}"))?;
    let packet = EndpointInfo::new(secret_key.public())
        .with_user_data(Some(probe_tag))
        .to_pkarr_signed_packet(&secret_key, DEFAULT_PKARR_TTL)
        .map_err(|e| format!("Failed to build probe record: {e}"))?;

    let started = Instant::now();
    let round_trip = async {
        client
            .publish(&packet)
            .await
            .map_err(|e| format!("Discovery server rejected the publish (PUT): {e}"))?;
        client.resolve(secret_key.public()).await.map_err(|e| {
            format!("Discovery server accepted the publish but lookup (GET) failed: {e}")
        })
    };
    let resolved = timeout(deadline, round_trip).await.map_err(|_| {
        format!(
            "Timed out waiting for the discovery server ({}s)",
            deadline.as_secs()
        )
    })??;
    if resolved != packet {
        return Err("Discovery server returned a different record than was published".to_string());
    }

    Ok(VerifyDiscoveryResponse {
        url: Some(pkarr_relay_url.to_string()),
        latency_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_discovery_mode_defaults() {
        let mode = build_discovery_mode(None).expect("none should default");
        assert!(matches!(mode, DiscoveryModeOption::Default));

        let mode = build_discovery_mode(Some(DiscoveryConfigArg {
            mode: "default".to_string(),
            pkarr_relay_url: None,
            dns_origin: None,
        }))
        .expect("default mode should parse");
        assert!(matches!(mode, DiscoveryModeOption::Default));
    }

    #[test]
    fn build_discovery_mode_custom_https() {
        let mode = build_discovery_mode(Some(DiscoveryConfigArg {
            mode: "custom".to_string(),
            pkarr_relay_url: Some("https://dns.example.com/pkarr".to_string()),
            dns_origin: None,
        }))
        .expect("custom https should parse");
        match mode {
            DiscoveryModeOption::Custom {
                pkarr_relay_url,
                dns_origin,
            } => {
                assert_eq!(pkarr_relay_url.as_str(), "https://dns.example.com/pkarr");
                assert!(dns_origin.is_none());
            }
            _ => panic!("expected custom discovery mode"),
        }
    }

    #[test]
    fn build_discovery_mode_custom_with_dns_origin() {
        let mode = build_discovery_mode(Some(DiscoveryConfigArg {
            mode: "custom".to_string(),
            pkarr_relay_url: Some("https://example.com/pkarr".to_string()),
            dns_origin: Some("Example.COM.".to_string()),
        }))
        .expect("custom with origin should parse");
        match mode {
            DiscoveryModeOption::Custom { dns_origin, .. } => {
                assert_eq!(dns_origin.as_deref(), Some("example.com"));
            }
            _ => panic!("expected custom discovery mode"),
        }
    }

    #[test]
    fn build_discovery_mode_custom_requires_url() {
        let err = build_discovery_mode(Some(DiscoveryConfigArg {
            mode: "custom".to_string(),
            pkarr_relay_url: None,
            dns_origin: None,
        }))
        .expect_err("custom without url should fail");
        assert!(err.contains("pkarr relay URL is required"));
    }

    #[test]
    fn build_discovery_mode_rejects_plain_http_non_loopback() {
        let err = build_discovery_mode(Some(DiscoveryConfigArg {
            mode: "custom".to_string(),
            pkarr_relay_url: Some("http://dns.example.com/pkarr".to_string()),
            dns_origin: None,
        }))
        .expect_err("plain http non-loopback should fail");
        assert!(err.contains("loopback"));
    }

    #[test]
    fn build_discovery_mode_allows_loopback_http() {
        let mode = build_discovery_mode(Some(DiscoveryConfigArg {
            mode: "custom".to_string(),
            pkarr_relay_url: Some("http://127.0.0.1:8080/pkarr".to_string()),
            dns_origin: None,
        }))
        .expect("loopback http should parse");
        assert!(matches!(mode, DiscoveryModeOption::Custom { .. }));
    }

    #[test]
    fn build_discovery_mode_rejects_embedded_credentials() {
        let err = build_discovery_mode(Some(DiscoveryConfigArg {
            mode: "custom".to_string(),
            pkarr_relay_url: Some("https://user:pass@dns.example.com/pkarr".to_string()),
            dns_origin: None,
        }))
        .expect_err("embedded credentials should fail");
        assert!(err.contains("username or password"));
        assert!(!err.contains("user:pass"));
    }

    #[test]
    fn build_discovery_mode_rejects_invalid_mode() {
        let err = build_discovery_mode(Some(DiscoveryConfigArg {
            mode: "nope".to_string(),
            pkarr_relay_url: None,
            dns_origin: None,
        }))
        .expect_err("invalid mode should fail");
        assert!(err.contains("Invalid discovery mode"));
    }

    #[test]
    fn parse_dns_origin_rejects_url_forms() {
        let err = parse_dns_origin(Some("https://example.com")).expect_err("url should fail");
        assert!(err.contains("hostname"));

        let err = parse_dns_origin(Some("example.com/pkarr")).expect_err("path should fail");
        assert!(err.contains("hostname"));

        let err = parse_dns_origin(Some("example.com:53")).expect_err("port should fail");
        assert!(err.contains("hostname"));
    }

    /// Minimal pkarr relay over loopback HTTP: PUT stores the body per path, GET
    /// returns it. Records every request so tests can assert what was sent.
    #[cfg(not(target_arch = "wasm32"))]
    mod pkarr_server {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        pub type Requests = Arc<Mutex<Vec<(String, String)>>>;

        pub async fn spawn(put_status: u16) -> (String, Requests) {
            spawn_with_delay(put_status, std::time::Duration::ZERO).await
        }

        pub async fn spawn_with_delay(
            put_status: u16,
            delay: std::time::Duration,
        ) -> (String, Requests) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/pkarr", listener.local_addr().unwrap());
            let requests: Requests = Default::default();
            let store: Arc<Mutex<HashMap<String, Vec<u8>>>> = Default::default();
            let seen = requests.clone();
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    tokio::spawn(handle(socket, put_status, delay, seen.clone(), store.clone()));
                }
            });
            (url, requests)
        }

        async fn handle(
            mut socket: TcpStream,
            put_status: u16,
            delay: std::time::Duration,
            seen: Requests,
            store: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        ) {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let mut request_line = head.lines().next().unwrap_or_default().split_whitespace();
            let method = request_line.next().unwrap_or_default().to_string();
            let path = request_line.next().unwrap_or_default().to_string();
            let content_length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buf.len() < header_end + content_length {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = buf[header_end..header_end + content_length].to_vec();
            seen.lock().unwrap().push((method.clone(), path.clone()));
            tokio::time::sleep(delay).await;

            let (status, payload) = match method.as_str() {
                "PUT" if put_status == 200 => {
                    store.lock().unwrap().insert(path, body);
                    (200, Vec::new())
                }
                "PUT" => (put_status, Vec::new()),
                "GET" => match store.lock().unwrap().get(&path) {
                    Some(stored) => (200, stored.clone()),
                    None => (404, Vec::new()),
                },
                _ => (405, Vec::new()),
            };
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                payload.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&payload).await;
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn custom_discovery_arg(url: &str) -> DiscoveryConfigArg {
        DiscoveryConfigArg {
            mode: "custom".to_string(),
            pkarr_relay_url: Some(url.to_string()),
            dns_origin: None,
        }
    }

    /// The test must exercise the self-hosted server itself — publish and
    /// lookup — not merely whether some relay is reachable.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn verify_discovery_publishes_then_resolves_on_the_custom_server() {
        let (url, requests) = pkarr_server::spawn(200).await;

        verify_discovery(custom_discovery_arg(&url))
            .await
            .expect("a working pkarr server should verify");

        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "expected one PUT and one GET: {requests:?}");
        assert_eq!(requests[0].0, "PUT");
        assert_eq!(requests[1].0, "GET");
        assert!(requests[0].1.starts_with("/pkarr/"));
        assert_eq!(requests[0].1, requests[1].1);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn verify_discovery_fails_when_the_server_rejects_publish() {
        let (url, _requests) = pkarr_server::spawn(500).await;

        let err = verify_discovery(custom_discovery_arg(&url))
            .await
            .expect_err("a rejected publish must not verify");
        assert!(err.contains("publish"), "unexpected error: {err}");
    }

    /// One deadline covers PUT + GET, so a slow server can't double the wait.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn verify_discovery_shares_one_deadline_across_publish_and_lookup() {
        let step = Duration::from_millis(300);
        let (url, _requests) = pkarr_server::spawn_with_delay(200, step).await;

        let err = verify_discovery_within(custom_discovery_arg(&url), Duration::from_millis(500))
            .await
            .expect_err("two 300ms steps must not fit a 500ms deadline");
        assert!(err.contains("Timed out"), "unexpected error: {err}");
    }

    #[test]
    fn parse_dns_origin_empty_is_none() {
        assert_eq!(parse_dns_origin(None).unwrap(), None);
        assert_eq!(parse_dns_origin(Some("")).unwrap(), None);
        assert_eq!(parse_dns_origin(Some("   ")).unwrap(), None);
    }
}
