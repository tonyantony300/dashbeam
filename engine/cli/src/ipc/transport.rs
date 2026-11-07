//! Platform transport for the node socket. Unix domain sockets, or a Windows
//! named pipe addressed by the same `node.sock` path.

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

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use tokio::sync::Mutex;

    /// `ERROR_PIPE_BUSY`: the server exists but every instance is taken.
    const ERROR_PIPE_BUSY: i32 = 231;

    pub struct Stream {
        kind: Kind,
    }

    enum Kind {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    pub type ReadHalf = tokio::io::ReadHalf<Stream>;
    pub type WriteHalf = tokio::io::WriteHalf<Stream>;

    pub struct Listener {
        name: String,
        slot: Mutex<NamedPipeServer>,
    }

    /// Named-pipe path for `node.sock`. Hashed so the name stays inside the
    /// 256-character pipe limit and does not depend on `Hash` randomization.
    pub fn pipe_name(path: &Path) -> String {
        let mut hash: u64 = 0xcbf29ce484222325;
        for b in path.as_os_str().as_encoded_bytes() {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!(r"\\.\pipe\dashbeam-{hash:016x}")
    }

    pub fn check_path(path: &Path) -> Result<()> {
        let name = pipe_name(path);
        // `\\.\pipe\` + name must fit in the kernel's 256-char pipe path.
        if name.len() > 256 {
            anyhow::bail!(
                "socket path {} is too long for a named pipe; use a shorter --data-dir",
                path.display()
            );
        }
        Ok(())
    }

    /// Binds the pipe for `path` and creates that path as a marker file.
    /// The caller must hold the node lock: an existing file is a stale marker.
    /// The pipe rejects remote clients. Its DACL is the process default
    /// (current user, plus local administrators).
    pub fn bind(path: &Path) -> Result<Listener> {
        check_path(path)?;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
        let name = pipe_name(path);
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name)
            .with_context(|| format!("bind {name}"))?;
        std::fs::write(path, &name).with_context(|| format!("create {}", path.display()))?;
        Ok(Listener {
            name,
            slot: Mutex::new(server),
        })
    }

    impl Listener {
        pub async fn accept(&self) -> std::io::Result<Stream> {
            let mut slot = self.slot.lock().await;
            slot.connect().await?;
            let next = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            let connected = std::mem::replace(&mut *slot, next);
            Ok(Stream {
                kind: Kind::Server(connected),
            })
        }
    }

    pub async fn connect(path: &Path) -> std::io::Result<Stream> {
        if let Err(e) = check_path(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ));
        }
        if !path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no node socket at {}", path.display()),
            ));
        }
        let name = pipe_name(path);
        let mut last = None;
        for _ in 0..50 {
            match ClientOptions::new().open(&name) {
                Ok(client) => {
                    return Ok(Stream {
                        kind: Kind::Client(client),
                    });
                }
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    last = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "named pipe busy")
        }))
    }

    pub fn split(s: Stream) -> (ReadHalf, WriteHalf) {
        tokio::io::split(s)
    }

    pub fn is_not_running(e: &std::io::Error) -> bool {
        matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    }

    impl AsyncRead for Stream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            match &mut self.kind {
                Kind::Server(s) => Pin::new(s).poll_read(cx, buf),
                Kind::Client(s) => Pin::new(s).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            match &mut self.kind {
                Kind::Server(s) => Pin::new(s).poll_write(cx, buf),
                Kind::Client(s) => Pin::new(s).poll_write(cx, buf),
            }
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            match &mut self.kind {
                Kind::Server(s) => Pin::new(s).poll_flush(cx),
                Kind::Client(s) => Pin::new(s).poll_flush(cx),
            }
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            match &mut self.kind {
                Kind::Server(s) => Pin::new(s).poll_shutdown(cx),
                Kind::Client(s) => Pin::new(s).poll_shutdown(cx),
            }
        }
    }
}

#[cfg(windows)]
pub use imp::*;
