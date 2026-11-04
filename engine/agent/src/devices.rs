//! Resolve a user-supplied device query to exactly one paired device.

use engine::PairedDeviceInfo;
use std::fmt;

/// Shortest endpoint-id prefix accepted as a device query.
const MIN_PREFIX_LEN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound {
        query: String,
    },
    Ambiguous {
        query: String,
        /// `(display name, endpoint id)` of every match.
        candidates: Vec<(String, String)>,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { query } => write!(f, "no paired device matches \"{query}\""),
            Self::Ambiguous { query, candidates } => {
                write!(
                    f,
                    "\"{query}\" matches {} paired devices; use an endpoint id:",
                    candidates.len()
                )?;
                for (name, id) in candidates {
                    write!(f, "\n  {name} ({id})")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Exact endpoint id, then case-insensitive display name, then a unique
/// endpoint-id prefix of at least 4 chars. Never guesses between matches.
pub fn resolve_device(
    devices: &[PairedDeviceInfo],
    query: &str,
) -> Result<PairedDeviceInfo, ResolveError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(ResolveError::NotFound {
            query: query.to_string(),
        });
    }

    if let Some(d) = devices
        .iter()
        .find(|d| d.endpoint_id.eq_ignore_ascii_case(q))
    {
        return Ok(d.clone());
    }

    let by_name: Vec<&PairedDeviceInfo> = devices
        .iter()
        .filter(|d| d.display_name.trim().to_lowercase() == q.to_lowercase())
        .collect();
    if let Some(d) = pick_one(query, &by_name)? {
        return Ok(d);
    }

    if q.len() >= MIN_PREFIX_LEN {
        let lower = q.to_lowercase();
        let by_prefix: Vec<&PairedDeviceInfo> = devices
            .iter()
            .filter(|d| d.endpoint_id.to_lowercase().starts_with(&lower))
            .collect();
        if let Some(d) = pick_one(query, &by_prefix)? {
            return Ok(d);
        }
    }

    Err(ResolveError::NotFound {
        query: query.to_string(),
    })
}

fn pick_one(
    query: &str,
    matches: &[&PairedDeviceInfo],
) -> Result<Option<PairedDeviceInfo>, ResolveError> {
    match matches {
        [] => Ok(None),
        [one] => Ok(Some((*one).clone())),
        many => Err(ResolveError::Ambiguous {
            query: query.to_string(),
            candidates: many
                .iter()
                .map(|d| (d.display_name.clone(), d.endpoint_id.clone()))
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, id: &str) -> PairedDeviceInfo {
        PairedDeviceInfo {
            endpoint_id: id.to_string(),
            display_name: name.to_string(),
            device_type: "laptop".to_string(),
            os: "macos".to_string(),
            paired_at: 0,
            last_seen_at: 0,
            relay_url: None,
            pairing_status: Default::default(),
            trusted: false,
            online: false,
        }
    }

    fn fixture() -> Vec<PairedDeviceInfo> {
        vec![
            dev("MacBook", "aaaa1111"),
            dev("Pixel", "bbbb2222"),
            dev("Desk", "aaab3333"),
        ]
    }

    #[test]
    fn resolves_by_exact_id() {
        let d = resolve_device(&fixture(), "bbbb2222").unwrap();
        assert_eq!(d.display_name, "Pixel");
    }

    #[test]
    fn resolves_by_case_insensitive_name() {
        let d = resolve_device(&fixture(), "macbook").unwrap();
        assert_eq!(d.endpoint_id, "aaaa1111");
    }

    #[test]
    fn resolves_by_unique_prefix() {
        let d = resolve_device(&fixture(), "bbbb").unwrap();
        assert_eq!(d.display_name, "Pixel");
    }

    #[test]
    fn ambiguous_prefix_is_rejected() {
        let err = resolve_device(&fixture(), "aaa").unwrap_err();
        // Under 4 chars: never treated as a prefix.
        assert!(matches!(err, ResolveError::NotFound { .. }));
        let mut devices = fixture();
        devices.push(dev("Other", "aaaa9999"));
        let err = resolve_device(&devices, "aaaa").unwrap_err();
        assert!(
            matches!(err, ResolveError::Ambiguous { ref candidates, .. } if candidates.len() == 2)
        );
    }

    #[test]
    fn ambiguous_name_lists_both_candidates() {
        let devices = vec![dev("MacBook", "aaaa1111"), dev("macbook", "cccc4444")];
        let err = resolve_device(&devices, "MacBook").unwrap_err();
        match &err {
            ResolveError::Ambiguous { candidates, .. } => assert_eq!(candidates.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("aaaa1111"), "{msg}");
        assert!(msg.contains("cccc4444"), "{msg}");
    }

    #[test]
    fn short_prefix_rejected() {
        let err = resolve_device(&fixture(), "bbb").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound { .. }));
    }

    #[test]
    fn unknown_is_not_found() {
        let err = resolve_device(&fixture(), "Toaster").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound { ref query } if query == "Toaster"));
        assert!(err.to_string().contains("Toaster"));
    }
}
