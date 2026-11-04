//! Human approval hook for pairing requests and inbound invites.

/// Re-exported so implementors don't need their own `async-trait` dependency.
pub use async_trait::async_trait;

/// Decides requests that policy alone can't settle. Implementations must not
/// block the runtime; a non-interactive node uses [`DenyAll`].
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve_pairing(&self, name: &str, endpoint_id: &str) -> bool;
    async fn approve_invite(
        &self,
        name: &str,
        endpoint_id: &str,
        file_count: u32,
        total_size: u64,
    ) -> bool;
}

/// Default for non-interactive nodes: says no to everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

#[async_trait]
impl Approver for DenyAll {
    async fn approve_pairing(&self, _name: &str, _endpoint_id: &str) -> bool {
        false
    }

    async fn approve_invite(
        &self,
        _name: &str,
        _endpoint_id: &str,
        _file_count: u32,
        _total_size: u64,
    ) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn deny_all_denies() {
        let a: Arc<dyn Approver> = Arc::new(DenyAll);
        assert!(!a.approve_pairing("x", "id").await);
        assert!(!a.approve_invite("x", "id", 1, 10).await);
    }
}
