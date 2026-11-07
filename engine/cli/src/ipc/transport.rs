//! Platform transport for the node socket: a Unix domain socket at
//! `node.sock`, or on Windows a named pipe whose name `node.sock` records.

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
    //! A named pipe with a random name, recorded in the `node.sock` marker
    //! file. The pipe's DACL admits only the current user, it rejects remote
    //! clients, and clients refuse a pipe whose owner is not the current user,
    //! so another local user can neither connect nor squat the name.

    use anyhow::{Context, Result};
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::pin::Pin;
    use std::ptr::null_mut;
    use std::task::{Context as TaskContext, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use tokio::sync::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        EqualSid, GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Every node pipe name starts with this. A marker naming anything else
    /// (e.g. `\\host\pipe\…` on another machine) is ignored.
    const PIPE_PREFIX: &str = r"\\.\pipe\dashbeam-";
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
        /// NUL-terminated SDDL, reused for every pipe instance.
        sddl: Vec<u16>,
        slot: Mutex<NamedPipeServer>,
    }

    /// Pipe names don't derive from the data dir path, so any path works.
    pub fn check_path(_path: &Path) -> Result<()> {
        Ok(())
    }

    /// Creates a fresh pipe and records its name in the marker at `path`.
    /// The caller must hold the node lock: an existing marker is stale.
    pub fn bind(path: &Path) -> Result<Listener> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
        let name = format!("{PIPE_PREFIX}{}", uuid::Uuid::new_v4().simple());
        let sddl = owner_only_sddl().context("read the current user's SID")?;
        let server = create(&name, &sddl, true).with_context(|| format!("bind {name}"))?;
        // Write then rename, so a client never reads a half-written name.
        let tmp = path.with_extension("sock.tmp");
        std::fs::write(&tmp, &name).with_context(|| format!("create {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("create {}", path.display()))?;
        Ok(Listener {
            name,
            sddl,
            slot: Mutex::new(server),
        })
    }

    impl Listener {
        pub async fn accept(&self) -> io::Result<Stream> {
            let mut slot = self.slot.lock().await;
            slot.connect().await?;
            let next = create(&self.name, &self.sddl, false)?;
            let connected = std::mem::replace(&mut *slot, next);
            Ok(Stream {
                kind: Kind::Server(connected),
            })
        }
    }

    pub async fn connect(path: &Path) -> io::Result<Stream> {
        let marker = std::fs::read_to_string(path)?;
        // A garbled marker counts as "no node": the starter replaces it.
        let name = parse_marker(&marker).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} does not name a dashbeam pipe", path.display()),
            )
        })?;
        let mut busy_retries = 50;
        let client = loop {
            match ClientOptions::new().open(name) {
                Ok(client) => break client,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && busy_retries > 0 => {
                    busy_retries -= 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        };
        verify_owner(&client)?;
        Ok(Stream {
            kind: Kind::Client(client),
        })
    }

    fn parse_marker(marker: &str) -> Option<&str> {
        let name = marker.trim();
        let id = name.strip_prefix(PIPE_PREFIX)?;
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(name)
    }

    pub fn split(s: Stream) -> (ReadHalf, WriteHalf) {
        tokio::io::split(s)
    }

    pub fn is_not_running(e: &io::Error) -> bool {
        matches!(
            e.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    }

    fn create(name: &str, sddl: &[u16], first: bool) -> io::Result<NamedPipeServer> {
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `sddl` is NUL-terminated UTF-16; `sd` is freed below.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        // SAFETY: `sa` and the descriptor it points to outlive the call.
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut sa as *mut SECURITY_ATTRIBUTES).cast(),
                )
        };
        // SAFETY: `sd` was allocated by the conversion above.
        unsafe { LocalFree(sd) };
        server
    }

    /// The current user's `TOKEN_USER`, in an 8-byte-aligned buffer.
    struct UserSid(Vec<u64>);

    impl UserSid {
        fn current() -> io::Result<Self> {
            let mut token: HANDLE = null_mut();
            // SAFETY: plain Win32 calls; the token handle is closed below.
            unsafe {
                if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut len = 0u32;
                GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
                let mut buf = vec![0u64; (len as usize).div_ceil(8)];
                let ok =
                    GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
                let err = io::Error::last_os_error();
                CloseHandle(token);
                if ok == 0 {
                    return Err(err);
                }
                Ok(Self(buf))
            }
        }

        fn psid(&self) -> PSID {
            // SAFETY: the buffer holds a TOKEN_USER filled by GetTokenInformation.
            unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
        }
    }

    /// `O:<me>D:P(A;;GA;;;<me>)`: owned by and open only to the current user.
    fn owner_only_sddl() -> io::Result<Vec<u16>> {
        let me = UserSid::current()?;
        let mut s: *mut u16 = null_mut();
        // SAFETY: `s` is a NUL-terminated LocalAlloc'd string, freed below.
        let sid = unsafe {
            if ConvertSidToStringSidW(me.psid(), &mut s) == 0 {
                return Err(io::Error::last_os_error());
            }
            let len = (0..).take_while(|&i| *s.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
            LocalFree(s.cast());
            sid
        };
        Ok(format!("O:{sid}D:P(A;;GA;;;{sid})")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect())
    }

    /// Refuses a pipe someone else created under our name: the node makes the
    /// current user its owner, so any other owner is an impostor.
    fn verify_owner(client: &NamedPipeClient) -> io::Result<()> {
        let me = UserSid::current()?;
        let mut owner: PSID = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `owner` points into `sd`, which is freed after the compare.
        let same = unsafe {
            let rc = GetSecurityInfo(
                client.as_raw_handle() as HANDLE,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut sd,
            );
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            let same = EqualSid(owner, me.psid()) != 0;
            LocalFree(sd);
            same
        };
        if same {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the node pipe belongs to another user; refusing to connect",
            ))
        }
    }

    impl AsyncRead for Stream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
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
        ) -> Poll<io::Result<usize>> {
            match &mut self.kind {
                Kind::Server(s) => Pin::new(s).poll_write(cx, buf),
                Kind::Client(s) => Pin::new(s).poll_write(cx, buf),
            }
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            match &mut self.kind {
                Kind::Server(s) => Pin::new(s).poll_flush(cx),
                Kind::Client(s) => Pin::new(s).poll_flush(cx),
            }
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<io::Result<()>> {
            match &mut self.kind {
                Kind::Server(s) => Pin::new(s).poll_shutdown(cx),
                Kind::Client(s) => Pin::new(s).poll_shutdown(cx),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::parse_marker;

        #[test]
        fn marker_must_name_a_local_dashbeam_pipe() {
            let ok = r"\\.\pipe\dashbeam-0123abcd";
            assert_eq!(parse_marker(&format!("{ok}\r\n")), Some(ok));
            assert_eq!(parse_marker(r"\\.\pipe\dashbeam-"), None);
            assert_eq!(parse_marker(r"\\host\pipe\dashbeam-0123abcd"), None);
            assert_eq!(parse_marker(r"\\.\pipe\other-0123abcd"), None);
            assert_eq!(parse_marker(r"\\.\pipe\dashbeam-..\x"), None);
        }
    }
}

#[cfg(windows)]
pub use imp::*;
