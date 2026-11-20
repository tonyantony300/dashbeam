//! `std::time::Instant` and `tokio::time` panic on `wasm32-unknown-unknown`.
//! Use `web-time` for monotonic clocks in the browser.

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::{Duration, Instant};

#[cfg(target_arch = "wasm32")]
pub use web_time::{Duration, Instant};

#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// Yield-based sleep that stays `Send` (required by iroh protocol handlers on wasm).
#[cfg(target_arch = "wasm32")]
pub async fn sleep(duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn timeout<F, T>(duration: Duration, future: F) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(duration, future).await
}

/// Real deadline in the browser: `n0_future`'s timer wraps `setTimeout` in a
/// `SendWrapper`, so it stays `Send` for iroh protocol handlers.
#[cfg(target_arch = "wasm32")]
pub async fn timeout<F, T>(
    duration: Duration,
    future: F,
) -> Result<T, n0_future::time::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    n0_future::time::timeout(duration, future).await
}
