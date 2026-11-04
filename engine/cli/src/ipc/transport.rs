//! Platform transport for the node socket. Unix domain sockets today; a
//! Windows named-pipe implementation would provide the same items.

#[cfg(unix)]
mod imp {
    use anyhow::{Context, Result};
    use std::path::Path;
    use tokio::net::{UnixListener, UnixStream};

    pub type Stream = UnixStream;
    pub type ReadHalf = tokio::net::unix::OwnedReadHalf;
    pub type WriteHalf = tokio::net::unix::OwnedWriteHalf;

    pub struct Listener(UnixListener);

    /// Unix socket paths are limited to `sun_path` (104 bytes on macOS, 108
    /// on Linux, including the NUL); fail early with an actionable message.
    pub fn check_path(path: &Path) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
        let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let max = addr.sun_path.len() - 1;
        let len = path.as_os_str().as_bytes().len();
        if len > max {
            anyhow::bail!(
                "socket path {} is too long for a Unix socket ({len} bytes, max {max}); use a shorter --data-dir",
                path.display()
            );
        }
        Ok(())
    }

    /// Binds `path` and restricts it to the owner (`0600`). The caller must
    /// hold the node lock: any file already at `path` is a stale socket.
    pub fn bind(path: &Path) -> Result<Listener> {
        use std::os::unix::fs::PermissionsExt;
        check_path(path)?;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
        let listener =
            UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;
        Ok(Listener(listener))
    }

    impl Listener {
        pub async fn accept(&self) -> std::io::Result<Stream> {
            self.0.accept().await.map(|(s, _)| s)
        }
    }

    pub async fn connect(path: &Path) -> std::io::Result<Stream> {
        if let Err(e) = check_path(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ));
        }
        UnixStream::connect(path).await
    }

    pub fn split(s: Stream) -> (ReadHalf, WriteHalf) {
        s.into_split()
    }

    /// Connect errors meaning "no node is serving this socket".
    pub fn is_not_running(e: &std::io::Error) -> bool {
        matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    }
}

#[cfg(unix)]
pub use imp::*;
