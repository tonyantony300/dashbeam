//! TLS trust helpers for iroh endpoint builders.
//!
//! OS trust is applied ONLY for user-configured self-hosted HTTPS infra
//! (custom relay / custom pkarr). n0's public infra is covered by iroh's
//! embedded Mozilla roots, and its relay hostnames are FQDNs with a trailing
//! root label ("use1-1.relay.n0.iroh.link.") that the Windows platform
//! verifier rejects with NotValidForName, because CERT_CHAIN_POLICY_SSL
//! matches pwszServerName literally against the certificate SANs.
//!
//! When OS trust is used, the platform verifier is wrapped so a trailing
//! root label is stripped before name matching (DNS-equivalent, no weaker).

use iroh::endpoint::Builder;
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
use std::sync::Arc;

use crate::types::{DiscoveryModeOption, RelayModeOption};

/// True when the user configured self-hosted discovery and/or relay HTTPS.
pub fn uses_custom_infra(discovery: &DiscoveryModeOption, relay: &RelayModeOption) -> bool {
    matches!(discovery, DiscoveryModeOption::Custom { .. })
        || matches!(relay, RelayModeOption::Custom { .. })
}

/// Apply OS CA trust only when `custom_infra` is set; otherwise keep iroh's
/// embedded Mozilla roots (pre-0.6.2 default-path behaviour).
///
/// On wasm32 and Android this is always a no-op (embedded roots).
pub fn with_system_ca_if_custom(builder: Builder, custom_infra: bool) -> Builder {
    if !custom_infra {
        return builder;
    }
    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    {
        apply_dot_tolerant_system_ca(builder)
    }
    #[cfg(any(target_arch = "wasm32", target_os = "android"))]
    {
        builder
    }
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn apply_dot_tolerant_system_ca(builder: Builder) -> Builder {
    builder.ca_tls_config(dot_tolerant_system_ca())
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn dot_tolerant_system_ca() -> iroh::tls::CaTlsConfig {
    use iroh::tls::CaTlsConfig;

    CaTlsConfig::custom_server_cert_verifier(Arc::new(|provider| {
        let inner = CaTlsConfig::system().server_cert_verifier(provider)?;
        Ok(Arc::new(DotTolerant(inner)) as Arc<dyn rustls::client::danger::ServerCertVerifier>)
    }))
}

/// rustls client config for self-hosted HTTPS infra, with the same trust
/// [`with_system_ca_if_custom`] gives endpoints (embedded roots on Android).
#[cfg(not(target_arch = "wasm32"))]
pub fn custom_infra_client_config() -> std::io::Result<rustls::ClientConfig> {
    #[cfg(not(target_os = "android"))]
    let ca = dot_tolerant_system_ca();
    #[cfg(target_os = "android")]
    let ca = iroh::tls::CaTlsConfig::default();
    ca.client_config(iroh::tls::default_provider())
}

/// Strips a trailing DNS root label before delegating to the inner verifier.
///
/// Windows' CERT_CHAIN_POLICY_SSL matches the reference name literally against
/// certificate SANs; `host.example.com.` does not match `host.example.com` or
/// `*.example.com`. Other platforms already normalize this.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
#[derive(Debug)]
struct DotTolerant(Arc<dyn rustls::client::danger::ServerCertVerifier>);

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
impl rustls::client::danger::ServerCertVerifier for DotTolerant {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Some(normalized) = strip_trailing_root_label(server_name) {
            return self.0.verify_server_cert(
                end_entity,
                intermediates,
                &normalized,
                ocsp_response,
                now,
            );
        }
        self.0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.0.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[rustls::DistinguishedName]> {
        self.0.root_hint_subjects()
    }
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn strip_trailing_root_label(
    server_name: &rustls::pki_types::ServerName<'_>,
) -> Option<rustls::pki_types::ServerName<'static>> {
    use rustls::pki_types::ServerName;

    if let ServerName::DnsName(dns) = server_name {
        let name = dns.as_ref();
        if let Some(stripped) = name.strip_suffix('.') {
            if !stripped.is_empty() {
                return ServerName::try_from(stripped.to_owned()).ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::types::{DiscoveryModeOption, RelayModeOption};

    #[test]
    fn uses_custom_infra_defaults_false() {
        assert!(!uses_custom_infra(
            &DiscoveryModeOption::Default,
            &RelayModeOption::Default
        ));
        assert!(!uses_custom_infra(
            &DiscoveryModeOption::Default,
            &RelayModeOption::Disabled
        ));
    }

    #[test]
    fn uses_custom_infra_true_for_custom_discovery_or_relay() {
        let pkarr = url::Url::parse("https://pkarr.example.com").unwrap();
        let relay_url =
            iroh::RelayUrl::from_str("https://relay.example.com").unwrap();

        assert!(uses_custom_infra(
            &DiscoveryModeOption::Custom {
                pkarr_relay_url: pkarr.clone(),
                dns_origin: None,
            },
            &RelayModeOption::Default
        ));
        assert!(uses_custom_infra(
            &DiscoveryModeOption::Default,
            &RelayModeOption::Custom {
                urls: vec![relay_url],
                auth_token: None,
            }
        ));
    }

    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    #[test]
    fn strip_trailing_root_label_normalizes_fqdn() {
        use rustls::pki_types::ServerName;

        let with_dot = ServerName::try_from("host.example.com.").expect("valid FQDN");
        let stripped = strip_trailing_root_label(&with_dot).expect("should strip");
        assert_eq!(stripped.to_str(), "host.example.com");

        let plain = ServerName::try_from("host.example.com").expect("valid name");
        assert!(strip_trailing_root_label(&plain).is_none());
    }

    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    #[test]
    fn dot_tolerant_passes_stripped_name_to_inner() {
        use std::sync::{Arc, Mutex};

        use rustls::client::danger::{
            HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
        };
        use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
        use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

        #[derive(Debug)]
        struct Recording(Mutex<Option<String>>);

        impl ServerCertVerifier for Recording {
            fn verify_server_cert(
                &self,
                _end_entity: &CertificateDer<'_>,
                _intermediates: &[CertificateDer<'_>],
                server_name: &ServerName<'_>,
                _ocsp_response: &[u8],
                _now: UnixTime,
            ) -> Result<ServerCertVerified, TlsError> {
                *self.0.lock().unwrap() = Some(server_name.to_str().into_owned());
                Ok(ServerCertVerified::assertion())
            }

            fn verify_tls12_signature(
                &self,
                _message: &[u8],
                _cert: &CertificateDer<'_>,
                _dss: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, TlsError> {
                Ok(HandshakeSignatureValid::assertion())
            }

            fn verify_tls13_signature(
                &self,
                _message: &[u8],
                _cert: &CertificateDer<'_>,
                _dss: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, TlsError> {
                Ok(HandshakeSignatureValid::assertion())
            }

            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                vec![]
            }
        }

        let inner = Arc::new(Recording(Mutex::new(None)));
        let wrapper = DotTolerant(inner.clone());
        let name = ServerName::try_from("host.example.com.").unwrap();
        let empty = CertificateDer::from(Vec::new());
        wrapper
            .verify_server_cert(
                &empty,
                &[],
                &name,
                &[],
                UnixTime::since_unix_epoch(std::time::Duration::from_secs(0)),
            )
            .unwrap();

        assert_eq!(
            inner.0.lock().unwrap().as_deref(),
            Some("host.example.com")
        );
    }
}
