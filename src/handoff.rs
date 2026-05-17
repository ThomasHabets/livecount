/// AI coded code for getting the connection from sni-router.
use std::ffi::CString;
use std::fs;
use std::io;
use std::io::Cursor;
use std::mem;
use std::net::TcpStream as StdTcpStream;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixDatagram as StdUnixDatagram;
use std::path::Path;
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll};

use futures_util::stream;
use log::warn;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UnixDatagram};

const MAX_INITIAL_DATA: usize = 1024 * 1024;
const CMSG_SPACE_FD: usize =
    unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as libc::c_uint) as usize };

#[repr(C)]
union CmsgSpace {
    header: libc::cmsghdr,
    bytes: [u8; CMSG_SPACE_FD],
}

pub type PrefixedTcpStream = PrefixedIo<TcpStream>;

pub struct PrefixedIo<T> {
    prefix: Cursor<Vec<u8>>,
    inner: T,
}

impl<T> PrefixedIo<T> {
    fn new(inner: T, prefix: Vec<u8>) -> Self {
        Self {
            prefix: Cursor::new(prefix),
            inner,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for PrefixedIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let pos = self.prefix.position() as usize;
        let prefix = self.prefix.get_ref();
        if pos < prefix.len() {
            let len = (prefix.len() - pos).min(buf.remaining());
            buf.put_slice(&prefix[pos..pos + len]);
            self.prefix.set_position((pos + len) as u64);
            return Poll::Ready(Ok(()));
        }

        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for PrefixedIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub fn bind(path: impl AsRef<Path>) -> io::Result<UnixDatagram> {
    let path = path.as_ref();
    match UnixDatagram::bind(path) {
        Ok(socket) => Ok(socket),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            remove_stale_socket(path)?;
            UnixDatagram::bind(path)
        }
        Err(err) => Err(err),
    }
}

pub fn configure_socket_path(
    path: impl AsRef<Path>,
    group: Option<&str>,
    mode: Option<u32>,
) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(group) = group {
        chgrp(path, group)?;
    }
    if let Some(mode) = mode {
        chmod(path, mode)?;
    }
    Ok(())
}

pub fn incoming(
    socket: UnixDatagram,
) -> impl futures_util::Stream<Item = io::Result<PrefixedTcpStream>> + Send {
    stream::unfold(socket, |socket| async move {
        let item = next_handoff(&socket).await;
        Some((item, socket))
    })
}

async fn next_handoff(socket: &UnixDatagram) -> io::Result<PrefixedTcpStream> {
    loop {
        match receive_handoff(socket).await {
            Ok(stream) => return Ok(stream),
            Err(err) => warn!("failed to receive socket handoff: {err}"),
        }
    }
}

async fn receive_handoff(socket: &UnixDatagram) -> io::Result<PrefixedTcpStream> {
    loop {
        socket.readable().await?;
        match recv_handoff(socket.as_raw_fd()) {
            Ok((initial_data, fd)) => {
                let stream = tcp_stream_from_fd(fd)?;
                return Ok(PrefixedIo::new(stream, initial_data));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(err),
        }
    }
}

fn tcp_stream_from_fd(fd: RawFd) -> io::Result<TcpStream> {
    // SAFETY: recv_handoff returns ownership of this descriptor via SCM_RIGHTS.
    let std_stream = unsafe { StdTcpStream::from_raw_fd(fd) };
    std_stream.set_nonblocking(true)?;
    TcpStream::from_std(std_stream)
}

fn recv_handoff(control_fd: RawFd) -> io::Result<(Vec<u8>, RawFd)> {
    let mut initial_data = vec![0; MAX_INITIAL_DATA];
    let mut iov = libc::iovec {
        iov_base: initial_data.as_mut_ptr().cast(),
        iov_len: initial_data.len(),
    };
    let mut control = CmsgSpace {
        bytes: [0; CMSG_SPACE_FD],
    };

    // SAFETY: msghdr is fully initialized before it is passed to recvmsg.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = unsafe { control.bytes.as_mut_ptr().cast() };
    msg.msg_controllen = CMSG_SPACE_FD;

    // SAFETY: msg points to valid data/control buffers for recvmsg to fill.
    let nread = unsafe { libc::recvmsg(control_fd, &mut msg, 0) };
    if nread < 0 {
        return Err(io::Error::last_os_error());
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff control message was truncated",
        ));
    }
    if msg.msg_flags & libc::MSG_TRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff initial data was truncated",
        ));
    }

    initial_data.truncate(nread as usize);
    let fd = extract_fd(&msg)?;
    if let Err(err) = set_cloexec(fd) {
        // SAFETY: fd was just received via SCM_RIGHTS and has not been
        // transferred to any Rust owner yet.
        unsafe {
            libc::close(fd);
        }
        return Err(err);
    }
    Ok((initial_data, fd))
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl is called with a valid descriptor received from the OS.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: fcntl is called with a valid descriptor and descriptor flags.
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn remove_stale_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{} exists and is not a Unix socket", path.display()),
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    }

    match StdUnixDatagram::unbound()?.connect(path) {
        Ok(()) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{} is already in use by another process", path.display()),
        )),
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
            warn!("removing stale Unix socket {}", path.display());
            fs::remove_file(path)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn chgrp(path: &Path, group: &str) -> io::Result<()> {
    let path = path_to_cstring(path)?;
    let gid = group_to_gid(group)?;

    // SAFETY: path is a valid C string. uid -1 means leave owner unchanged.
    let ret = unsafe { libc::chown(path.as_ptr(), !0 as libc::uid_t, gid) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    let path = path_to_cstring(path)?;

    // SAFETY: path is a valid C string and mode came from CLI parsing.
    let ret = unsafe { libc::chmod(path.as_ptr(), mode as libc::mode_t) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn path_to_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains an interior NUL byte: {err}"),
        )
    })
}

fn group_to_gid(group: &str) -> io::Result<libc::gid_t> {
    if let Ok(gid) = group.parse::<libc::gid_t>() {
        return Ok(gid);
    }

    let group = CString::new(group).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("group contains an interior NUL byte: {err}"),
        )
    })?;

    let mut buf_len = group_buffer_len();
    loop {
        let mut group_entry = mem::MaybeUninit::<libc::group>::uninit();
        let mut result = ptr::null_mut();
        let mut buf = vec![0; buf_len];

        // SAFETY: all pointers reference writable memory valid for this call.
        let ret = unsafe {
            libc::getgrnam_r(
                group.as_ptr(),
                group_entry.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut result,
            )
        };

        if ret == libc::ERANGE {
            buf_len *= 2;
            continue;
        }
        if ret != 0 {
            return Err(io::Error::from_raw_os_error(ret));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "group does not exist",
            ));
        }

        // SAFETY: getgrnam_r succeeded and initialized group_entry.
        return Ok(unsafe { group_entry.assume_init().gr_gid });
    }
}

fn group_buffer_len() -> usize {
    // SAFETY: sysconf has no memory safety requirements.
    let len = unsafe { libc::sysconf(libc::_SC_GETGR_R_SIZE_MAX) };
    if len > 0 {
        len as usize
    } else {
        16 * 1024
    }
}

fn extract_fd(msg: &libc::msghdr) -> io::Result<RawFd> {
    let mut found_fd = None;

    // SAFETY: msg was filled by recvmsg, and libc CMSG helpers walk only the
    // control buffer described by msg.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            let header = &*cmsg;
            if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
                let data_len = header.cmsg_len.saturating_sub(libc::CMSG_LEN(0) as usize);
                let fd_count = data_len / mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();

                for i in 0..fd_count {
                    let fd = data.add(i).read_unaligned();
                    if found_fd.is_none() {
                        found_fd = Some(fd);
                    } else {
                        libc::close(fd);
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }

    found_fd.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff did not include a file descriptor",
        )
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream as StdTcpStream};
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixDatagram as StdUnixDatagram;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{bind, receive_handoff, CmsgSpace, PrefixedIo, CMSG_SPACE_FD};

    static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);

    #[tokio::test]
    async fn reads_prefix_before_inner_stream() {
        let (client, mut server) = tokio::io::duplex(64);
        let mut stream = PrefixedIo::new(client, b"hello ".to_vec());

        server.write_all(b"world").await.unwrap();
        drop(server);

        let mut out = String::new();
        stream.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn receives_fd_and_initial_data() {
        let (control_tx, control_rx) = StdUnixDatagram::pair().unwrap();
        control_rx.set_nonblocking(true).unwrap();
        let control_rx = tokio::net::UnixDatagram::from_std(control_rx).unwrap();

        let (mut client, server) = tcp_pair();
        send_fd_with_data(control_tx.as_raw_fd(), server.as_raw_fd(), b"hello ").unwrap();
        drop(server);

        client.write_all(b"world").unwrap();
        drop(client);

        let mut stream = receive_handoff(&control_rx).await.unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn bind_removes_stale_socket_file() {
        let path = temp_socket_path("stale");
        cleanup_socket_path(&path);

        let stale = StdUnixDatagram::bind(&path).unwrap();
        drop(stale);

        let socket = bind(&path).unwrap();
        drop(socket);

        cleanup_socket_path(&path);
    }

    #[tokio::test]
    async fn bind_does_not_remove_active_socket_file() {
        let path = temp_socket_path("active");
        cleanup_socket_path(&path);

        let active = StdUnixDatagram::bind(&path).unwrap();
        let err = match bind(&path) {
            Ok(_) => panic!("bind unexpectedly replaced an active socket"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());

        drop(active);
        cleanup_socket_path(&path);
    }

    fn tcp_pair() -> (StdTcpStream, StdTcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = StdTcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _addr) = listener.accept().unwrap();
        (client, server)
    }

    fn temp_socket_path(name: &str) -> PathBuf {
        let next = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "livecount-{name}-{}-{next}.sock",
            std::process::id()
        ))
    }

    fn cleanup_socket_path(path: &PathBuf) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => panic!("failed to remove {}: {err}", path.display()),
        }
    }

    fn send_fd_with_data(
        socket_fd: libc::c_int,
        fd_to_send: libc::c_int,
        data: &[u8],
    ) -> std::io::Result<usize> {
        let mut data = data.to_vec();
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };
        let mut control = CmsgSpace {
            bytes: [0; CMSG_SPACE_FD],
        };

        // SAFETY: msghdr and cmsghdr are initialized below before sendmsg.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = unsafe { control.bytes.as_mut_ptr().cast() };
        msg.msg_controllen = CMSG_SPACE_FD;

        // SAFETY: msg has a valid control buffer large enough for one fd.
        let written = unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as libc::c_uint) as usize;
            libc::CMSG_DATA(cmsg)
                .cast::<libc::c_int>()
                .write(fd_to_send);
            libc::sendmsg(socket_fd, &msg, 0)
        };

        if written < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    }
}
