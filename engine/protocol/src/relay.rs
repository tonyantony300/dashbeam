use crate::time_compat::{sleep, timeout, Duration, Instant};
use crate::types::{get_or_create_secret, RelayModeOption};
use iroh::{endpoint::presets, Endpoint, Watcher};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct RelayConfigArg {
    pub mode: String,
    pub urls: Vec<String>,
    pub auth_token: Option<String>,
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayFallbackPolicy {
    Strict,
    Public,
}

const MAX_RELAY_URL_LENGTH: usize = 2048;
const MAX_RELAY_AUTH_TOKEN_LENGTH: usize = 4096;
const RELAY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

fn has_disallowed_relay_text_char(value: &str) -> bool {
    value
        .chars()
        .any(|char| char.is_control() || char.is_whitespace())
}

fn normalize_relay_auth_token(value: Option<String>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Err("Relay auth token must not be empty".to_string());
    }
    if value.len() > MAX_RELAY_AUTH_TOKEN_LENGTH {
        return Err("Relay auth token is too long".to_string());
    }
    if has_disallowed_relay_text_char(&value) {
        return Err(
            "Relay auth token must not contain whitespace or control characters".to_string(),
        );
    }
    Ok(Some(value))
}

fn is_loopback_relay_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    host.parse::<IpAddr>()
        .map(|addr| addr.is_loopback())
        .unwrap_or(false)
}

fn parse_relay_url_for_ipc(url: &str, has_auth_token: bool) -> Result<iroh::RelayUrl, String> {
    if url.is_empty() {
        return Err("Relay URL must not be empty".to_string());
    }
    if url.len() > MAX_RELAY_URL_LENGTH {
        return Err("Relay URL is too long".to_string());
    }
    if has_disallowed_relay_text_char(url) {
        return Err("Relay URL must not contain whitespace or control characters".to_string());
    }

    let relay_url = iroh::RelayUrl::from_str(url).map_err(|_| "Invalid relay URL".to_string())?;
    if relay_url.username() != "" || relay_url.password().is_some() {
        return Err("Relay URL must not include a username or password".to_string());
    }

    let host = relay_url
        .host_str()
        .ok_or_else(|| "Relay URL must include a host".to_string())?;

    match relay_url.scheme() {
        "https" => Ok(relay_url),
        "http" if !has_auth_token && is_loopback_relay_host(host) => Ok(relay_url),
        "http" if has_auth_token => {
            Err("Relay URLs must use https when an auth token is configured".to_string())
        }
        "http" => Err("Plain HTTP relay URLs are only allowed for loopback hosts".to_string()),
        _ => Err("Relay URL scheme must be https or loopback http".to_string()),
    }
}

pub fn build_relay_mode(arg: Option<RelayConfigArg>) -> Result<RelayModeOption, String> {
    match arg {
        None => Ok(RelayModeOption::Default),
        Some(arg) => match arg.mode.as_str() {
            "default" => Ok(RelayModeOption::Default),
            "disabled" => Ok(RelayModeOption::Disabled),
            "custom" => {
                if arg.urls.is_empty() {
                    return Err("At least one relay URL is required for custom mode".to_string());
                }
                let auth_token = normalize_relay_auth_token(arg.auth_token)?;
                let has_auth_token = auth_token.is_some();
                let urls = arg
                    .urls
                    .iter()
                    .map(|url| parse_relay_url_for_ipc(url, has_auth_token))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(RelayModeOption::Custom { urls, auth_token })
            }
            other => Err(format!("Invalid relay mode: {other}")),
        },
    }
}

pub fn relay_fallback_policy(arg: &RelayConfigArg) -> Result<RelayFallbackPolicy, String> {
    match arg.fallback.as_deref().unwrap_or("strict") {
        "strict" => Ok(RelayFallbackPolicy::Strict),
        "public" => Ok(RelayFallbackPolicy::Public),
        other => Err(format!("Invalid relay fallback policy: {other}")),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayStatusResponse {
    pub kind: String,
    pub url: Option<String>,
    pub connected: bool,
    pub fell_back_to_public: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRelaysResponse {
    pub url: Option<String>,
    pub latency_ms: u64,
}

pub fn is_public_relay_url(url: &str) -> bool {
    url.contains("relay.n0.iroh.link") || url.contains(".iroh.link")
}

fn connected_home_relay_url(endpoint: &Endpoint) -> Option<String> {
    endpoint
        .home_relay_status()
        .get()
        .into_iter()
        .find(|status| status.is_connected())
        .map(|status| status.url().to_string())
}

async fn connected_home_relay_url_after_online(endpoint: &Endpoint) -> Option<String> {
    for _ in 0..30 {
        if let Some(url) = connected_home_relay_url(endpoint) {
            return Some(url);
        }
        sleep(Duration::from_millis(100)).await;
    }
    connected_home_relay_url(endpoint)
}

fn relay_status_kind_for_url(url: &Option<String>, custom: bool) -> String {
    match url {
        Some(u) if is_public_relay_url(u) => "public".to_string(),
        Some(_) if custom => "custom".to_string(),
        Some(_) => "public".to_string(),
        None if custom => "custom".to_string(),
        None => "public".to_string(),
    }
}

async fn probe_relay_mode(relay_mode: RelayModeOption) -> Result<Option<String>, String> {
    if matches!(relay_mode, RelayModeOption::Disabled) {
        return Ok(None);
    }

    let secret_key = get_or_create_secret().map_err(|e| e.to_string())?;
    let custom_infra = matches!(relay_mode, RelayModeOption::Custom { .. });
    let endpoint =
        crate::tls_config::with_system_ca_if_custom(Endpoint::builder(presets::Minimal), custom_infra)
            .secret_key(secret_key)
            .relay_mode(relay_mode.into())
            .bind()
            .await
            .map_err(|e| format!("Failed to bind endpoint: {e}"))?;

    let online_result = timeout(RELAY_PROBE_TIMEOUT, endpoint.online()).await;

    online_result.map_err(|_| "Timed out waiting for relay connection".to_string())?;
    let url = connected_home_relay_url_after_online(&endpoint).await;
    endpoint.close().await;
    Ok(url)
}

fn apply_custom_relay_probe_result(
    preferred: RelayModeOption,
    fallback: RelayFallbackPolicy,
    probe_result: Result<Option<String>, String>,
) -> Result<(RelayModeOption, bool), String> {
    if probe_result.is_ok() {
        return Ok((preferred, false));
    }

    match fallback {
        RelayFallbackPolicy::Strict => {
            Err("Custom relay unreachable and strict fallback policy is enabled".to_string())
        }
        RelayFallbackPolicy::Public => {
            tracing::warn!(
                "Custom relay unreachable within {}s; falling back to public relays",
                RELAY_PROBE_TIMEOUT.as_secs()
            );
            Ok((RelayModeOption::Default, true))
        }
    }
}

/// Prefer configured custom relays; fall back to public relays only when selected.
pub async fn resolve_relay_mode_with_fallback(
    arg: Option<RelayConfigArg>,
) -> Result<(RelayModeOption, bool), String> {
    let fallback = arg
        .as_ref()
        .map(relay_fallback_policy)
        .transpose()?
        .unwrap_or(RelayFallbackPolicy::Strict);
    let preferred = build_relay_mode(arg)?;

    match &preferred {
        RelayModeOption::Disabled | RelayModeOption::Default => Ok((preferred, false)),
        RelayModeOption::Custom { .. } => {
            let probe =
                timeout(RELAY_PROBE_TIMEOUT, probe_relay_mode(preferred.clone()))
                    .await;

            let probe_result = match probe {
                Ok(result) => result,
                Err(_) => Err("Timed out waiting for relay connection".to_string()),
            };
            apply_custom_relay_probe_result(preferred, fallback, probe_result)
        }
    }
}

/// Relay mode for the long-lived device node. Unlike a one-off transfer, the
/// node outlives a relay outage, so strict custom relays apply without a probe:
/// iroh keeps retrying them, and failing here would leave the node on its
/// previous (possibly public) relays. Only `public` fallback needs a probe.
pub async fn resolve_node_relay_mode(
    arg: Option<RelayConfigArg>,
) -> Result<(RelayModeOption, bool), String> {
    let fallback = arg
        .as_ref()
        .map(relay_fallback_policy)
        .transpose()?
        .unwrap_or(RelayFallbackPolicy::Strict);
    if fallback == RelayFallbackPolicy::Public {
        return resolve_relay_mode_with_fallback(arg).await;
    }
    Ok((build_relay_mode(arg)?, false))
}

/// Check which relay the app can reach, with public fallback only when selected.
pub async fn get_relay_status(
    relay: Option<RelayConfigArg>,
) -> Result<RelayStatusResponse, String> {
    let fallback = relay
        .as_ref()
        .map(relay_fallback_policy)
        .transpose()?
        .unwrap_or(RelayFallbackPolicy::Strict);
    let preferred = build_relay_mode(relay.clone())?;

    if matches!(preferred, RelayModeOption::Disabled) {
        return Ok(RelayStatusResponse {
            kind: "disabled".to_string(),
            url: None,
            connected: false,
            fell_back_to_public: false,
        });
    }

    if let RelayModeOption::Custom { .. } = &preferred {
        let custom_probe =
            timeout(RELAY_PROBE_TIMEOUT, probe_relay_mode(preferred.clone())).await;

        if let Ok(Ok(url)) = custom_probe {
            return Ok(RelayStatusResponse {
                kind: relay_status_kind_for_url(&url, true),
                url,
                connected: true,
                fell_back_to_public: false,
            });
        }

        if matches!(fallback, RelayFallbackPolicy::Strict) {
            return Ok(RelayStatusResponse {
                kind: "unavailable".to_string(),
                url: None,
                connected: false,
                fell_back_to_public: false,
            });
        }

        tracing::warn!("Custom relay unreachable; checking public relay fallback");
        let public_probe = timeout(
            RELAY_PROBE_TIMEOUT,
            probe_relay_mode(RelayModeOption::Default),
        )
        .await;

        if let Ok(Ok(url)) = public_probe {
            return Ok(RelayStatusResponse {
                kind: "public".to_string(),
                url,
                connected: true,
                fell_back_to_public: true,
            });
        }

        return Ok(RelayStatusResponse {
            kind: "unavailable".to_string(),
            url: None,
            connected: false,
            fell_back_to_public: false,
        });
    }

    let public_probe = timeout(
        RELAY_PROBE_TIMEOUT,
        probe_relay_mode(RelayModeOption::Default),
    )
    .await;

    if let Ok(Ok(url)) = public_probe {
        return Ok(RelayStatusResponse {
            kind: "public".to_string(),
            url,
            connected: true,
            fell_back_to_public: false,
        });
    }

    Ok(RelayStatusResponse {
        kind: "unavailable".to_string(),
        url: None,
        connected: false,
        fell_back_to_public: false,
    })
}

/// Verify connectivity to configured relay servers.
pub async fn verify_relays(relay: RelayConfigArg) -> Result<VerifyRelaysResponse, String> {
    let relay_mode = build_relay_mode(Some(relay))?;

    if matches!(relay_mode, RelayModeOption::Disabled) {
        return Err("Relay verification requires default or custom relay mode".to_string());
    }

    let secret_key = get_or_create_secret().map_err(|e| e.to_string())?;
    let custom_infra = matches!(relay_mode, RelayModeOption::Custom { .. });

    let endpoint =
        crate::tls_config::with_system_ca_if_custom(Endpoint::builder(presets::Minimal), custom_infra)
            .secret_key(secret_key)
            .relay_mode(relay_mode.into())
            .bind()
            .await
            .map_err(|e| format!("Failed to bind endpoint: {e}"))?;

    let started = Instant::now();

    timeout(RELAY_PROBE_TIMEOUT, endpoint.online())
        .await
        .map_err(|_| {
            format!(
                "Timed out waiting for relay connection ({}s)",
                RELAY_PROBE_TIMEOUT.as_secs()
            )
        })?;

    let latency_ms = started.elapsed().as_millis() as u64;
    let url = connected_home_relay_url(&endpoint);

    endpoint.close().await;
    Ok(VerifyRelaysResponse { url, latency_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom_relay_arg(fallback: Option<&str>) -> RelayConfigArg {
        RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["https://relay.example.com".to_string()],
            auth_token: None,
            fallback: fallback.map(str::to_string),
        }
    }

    #[test]
    fn build_relay_mode_defaults_to_public() {
        let mode = build_relay_mode(None).expect("default mode should parse");
        assert!(matches!(mode, RelayModeOption::Default));
    }

    #[test]
    fn build_relay_mode_custom_with_auth() {
        let mode = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["https://relay.example.com".to_string()],
            auth_token: Some("secret".to_string()),
            fallback: None,
        }))
        .expect("custom mode should parse");

        match mode {
            RelayModeOption::Custom { urls, auth_token } => {
                assert_eq!(urls.len(), 1);
                assert_eq!(auth_token.as_deref(), Some("secret"));
            }
            _ => panic!("expected custom relay mode"),
        }
    }

    #[test]
    fn build_relay_mode_custom_requires_urls() {
        let err = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec![],
            auth_token: None,
            fallback: None,
        }))
        .expect_err("empty custom urls should fail");
        assert!(err.contains("At least one relay URL"));
    }

    #[test]
    fn build_relay_mode_rejects_auth_token_over_http() {
        let err = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["http://127.0.0.1:3340".to_string()],
            auth_token: Some("secret".to_string()),
            fallback: None,
        }))
        .expect_err("auth tokens must not be sent over cleartext relay urls");

        assert!(err.contains("https"));
    }

    #[test]
    fn build_relay_mode_allows_loopback_http_without_auth_token() {
        let mode = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["http://127.0.0.1:3340".to_string()],
            auth_token: None,
            fallback: None,
        }))
        .expect("loopback http relay is allowed for local development without auth");

        assert!(matches!(mode, RelayModeOption::Custom { .. }));
    }

    #[test]
    fn build_relay_mode_rejects_embedded_url_credentials_without_echoing_them() {
        let err = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["https://user:password@relay.example.com".to_string()],
            auth_token: None,
            fallback: None,
        }))
        .expect_err("relay urls must not carry embedded credentials");

        assert!(err.contains("username or password"));
        assert!(!err.contains("user:password"));
    }

    #[test]
    fn build_relay_mode_rejects_auth_token_whitespace() {
        let err = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["https://relay.example.com".to_string()],
            auth_token: Some("secret token".to_string()),
            fallback: None,
        }))
        .expect_err("bearer tokens must not contain whitespace");

        assert!(err.contains("auth token"));
    }

    #[test]
    fn build_relay_mode_rejects_auth_token_leading_or_trailing_whitespace() {
        let err = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["https://relay.example.com".to_string()],
            auth_token: Some(" secret ".to_string()),
            fallback: None,
        }))
        .expect_err("bearer tokens must not be silently trimmed");

        assert!(err.contains("auth token"));
    }

    #[test]
    fn build_relay_mode_rejects_empty_auth_token() {
        let err = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["https://relay.example.com".to_string()],
            auth_token: Some("".to_string()),
            fallback: None,
        }))
        .expect_err("explicitly empty bearer tokens must fail closed");

        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn build_relay_mode_rejects_blank_auth_token() {
        let err = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["https://relay.example.com".to_string()],
            auth_token: Some(" \t ".to_string()),
            fallback: None,
        }))
        .expect_err("blank bearer tokens must not be silently cleared");

        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn build_relay_mode_rejects_oversized_auth_token() {
        let err = build_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["https://relay.example.com".to_string()],
            auth_token: Some("a".repeat(MAX_RELAY_AUTH_TOKEN_LENGTH + 1)),
            fallback: None,
        }))
        .expect_err("bearer tokens must have a bounded size");

        assert!(err.contains("too long"));
    }

    #[test]
    fn relay_config_missing_fallback_defaults_to_strict() {
        let arg: RelayConfigArg = serde_json::from_str(
            r#"{"mode":"custom","urls":["https://relay.example.com"],"auth_token":null}"#,
        )
        .expect("old frontend payloads should still deserialize");

        assert_eq!(
            relay_fallback_policy(&arg).expect("policy should parse"),
            RelayFallbackPolicy::Strict
        );
    }

    #[test]
    fn strict_custom_relay_probe_failure_fails_closed() {
        let preferred = build_relay_mode(Some(custom_relay_arg(Some("strict"))))
            .expect("custom mode should parse");
        let err = apply_custom_relay_probe_result(
            preferred,
            RelayFallbackPolicy::Strict,
            Err("Timed out waiting for relay connection".to_string()),
        )
        .expect_err("strict fallback should fail closed");

        assert!(err.contains("Custom relay unreachable"));
    }

    #[tokio::test]
    async fn node_relay_mode_strict_keeps_unreachable_custom_relay() {
        // `.invalid` never resolves: a probe would fail, and strict must not
        // leave the node on whatever (public) relays it had before.
        let (mode, fell_back) = resolve_node_relay_mode(Some(RelayConfigArg {
            mode: "custom".to_string(),
            urls: vec!["https://relay.invalid".to_string()],
            auth_token: None,
            fallback: Some("strict".to_string()),
        }))
        .await
        .expect("strict custom relay must apply even when unreachable");

        assert!(matches!(mode, RelayModeOption::Custom { .. }));
        assert!(!fell_back);
    }

    #[test]
    fn public_custom_relay_probe_failure_falls_back_to_public() {
        let preferred = build_relay_mode(Some(custom_relay_arg(Some("public"))))
            .expect("custom mode should parse");
        let (mode, fell_back) = apply_custom_relay_probe_result(
            preferred,
            RelayFallbackPolicy::Public,
            Err("Timed out waiting for relay connection".to_string()),
        )
        .expect("public fallback should use default relays");

        assert!(matches!(mode, RelayModeOption::Default));
        assert!(fell_back);
    }
}
