//! Adopting raw descriptors, and handing them to another process.
//!
//! Two mechanisms are used, and they are easy to confuse:
//!
//! * **Inheritance** — a child can be given a descriptor at a *known number*
//!   (`dup2`). That is how the launcher keeps its listen sockets across the
//!   user-namespace re-exec, and how the `__dial` helper receives its control
//!   socketpair (fd 3) and the DNS sockets (fd 4, 5, ...).
//! * **`SCM_RIGHTS`** — an *already connected* socket can be sent over a Unix
//!   socket at any time. That is how the helper hands a proxied connection back
//!   to the manager.
//!
//! Every descriptor std creates is close-on-exec, so nothing leaks into the apps
//! we spawn by accident. Adoption functions here deliberately take ownership of
//! the descriptor (`from_raw_fd`), which is why they are `unsafe` to call — the
//! safety contract is documented once, on each function.

#![allow(unsafe_code)]

use std::io;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;

use super::cvt;

/// Closes a descriptor that nobody owns yet.
pub fn close(fd: RawFd) {
    if fd < 0 {
        return;
    }
    // SAFETY: closing a raw descriptor cannot violate memory safety; callers
    // only pass descriptors they received and have not wrapped in an owner.
    unsafe { libc::close(fd) };
}

/// Whether `fd` is open in this process.
pub fn is_open(fd: RawFd) -> bool {
    if fd < 0 {
        return false;
    }
    // SAFETY: F_GETFD only queries descriptor flags.
    (unsafe { libc::fcntl(fd, libc::F_GETFD) }) != -1
}

/// Marks `fd` close-on-exec.
pub fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) })?;
    Ok(())
}

/// Whether `fd` is close-on-exec. Mostly interesting for tests.
pub fn is_cloexec(fd: RawFd) -> bool {
    // SAFETY: F_GETFD only queries descriptor flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags != -1 && flags & libc::FD_CLOEXEC != 0
}

/// A socket option, read back as an integer.
fn sockopt(fd: RawFd, option: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value`/`length` describe a buffer of the right size, and
    // getsockopt only writes inside it. A non-socket returns ENOTSOCK.
    cvt(unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut length,
        )
    })?;
    Ok(value)
}

/// Checks that `fd` is an internet socket of the given kind before adopting it,
/// so a wrong descriptor is a clean error instead of a surprise later.
fn check_inet_socket(fd: RawFd, expected: libc::c_int, what: &str) -> io::Result<()> {
    let domain = sockopt(fd, libc::SO_DOMAIN)?;
    let kind = sockopt(fd, libc::SO_TYPE)?;
    if !matches!(domain, libc::AF_INET | libc::AF_INET6) || kind != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fd {fd} is not {what}"),
        ));
    }
    Ok(())
}

/// Takes ownership of an inherited listening socket.
///
/// This is the descriptor that travels across the user-namespace re-exec: bound
/// in the host namespace as the normal user, adopted again inside the child.
pub fn adopt_listener(fd: RawFd) -> io::Result<TcpListener> {
    check_inet_socket(fd, libc::SOCK_STREAM, "a TCP socket")?;
    if sockopt(fd, libc::SO_ACCEPTCONN)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fd {fd} is not a listening socket"),
        ));
    }

    // SAFETY: a validated, open, listening TCP socket that nobody else owns.
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    listener.set_nonblocking(true)?;
    // The re-exec cleared close-on-exec so the descriptor could travel here; the
    // apps we are about to spawn must not inherit the proxy's own listener.
    set_cloexec(fd)?;
    Ok(listener)
}

/// Takes ownership of a connected TCP socket received from the dialer helper.
pub fn adopt_stream(fd: RawFd) -> io::Result<TcpStream> {
    check_inet_socket(fd, libc::SOCK_STREAM, "a TCP socket")?;
    // SAFETY: a validated, open TCP socket that nobody else owns.
    Ok(unsafe { TcpStream::from_raw_fd(fd) })
}

/// Takes ownership of an inherited UDP socket (the DNS relays).
pub fn adopt_udp_socket(fd: RawFd) -> io::Result<UdpSocket> {
    check_inet_socket(fd, libc::SOCK_DGRAM, "a UDP socket")?;
    // SAFETY: a validated, open UDP socket that nobody else owns.
    Ok(unsafe { UdpSocket::from_raw_fd(fd) })
}

/// Takes ownership of an inherited Unix socket (the `__dial` control channel).
pub fn adopt_unix_stream(fd: RawFd) -> io::Result<UnixStream> {
    if sockopt(fd, libc::SO_DOMAIN)? != libc::AF_UNIX
        || sockopt(fd, libc::SO_TYPE)? != libc::SOCK_STREAM
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fd {fd} is not a Unix stream socket"),
        ));
    }
    // SAFETY: a validated, open Unix socket that nobody else owns.
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

/// Sends one byte plus one descriptor over a Unix socket (`SCM_RIGHTS`).
pub fn send_with_fd(socket: RawFd, byte: u8, fd: RawFd) -> io::Result<()> {
    let mut payload = [byte];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };

    let mut control = [0u8; 64];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) } as libc::size_t;

    // SAFETY: the control buffer is large enough for exactly one SCM_RIGHTS
    // message carrying one descriptor, which is what we build here.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&message);
        if cmsg.is_null() {
            return Err(io::Error::other("no room for SCM_RIGHTS"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len =
            libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as libc::size_t;
        std::ptr::copy_nonoverlapping(
            (&fd as *const RawFd).cast::<u8>(),
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<libc::c_int>(),
        );
    }

    loop {
        // SAFETY: the msghdr describes valid buffers for the duration of the call.
        let sent = unsafe { libc::sendmsg(socket, &message, 0) };
        if sent == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(());
    }
}

/// Receives one byte plus, optionally, one descriptor (`SCM_RIGHTS`).
///
/// Returns `Ok(None)` on a clean EOF, which means the peer exited.
pub fn recv_with_fd(socket: RawFd, byte: &mut u8) -> io::Result<Option<RawFd>> {
    let mut iov = libc::iovec {
        iov_base: (byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0u8; 64];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();

    loop {
        // SAFETY: the msghdr describes valid buffers for the duration of the call.
        let received = unsafe { libc::recvmsg(socket, &mut message, libc::MSG_CMSG_CLOEXEC) };
        if received == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if received == 0 {
            return Ok(None);
        }
        break;
    }

    // SAFETY: the kernel filled in msg_controllen; walk the messages it wrote.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&message);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let mut fd: RawFd = -1;
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg),
                    (&mut fd as *mut RawFd).cast::<u8>(),
                    std::mem::size_of::<libc::c_int>(),
                );
                return Ok(Some(fd));
            }
            cmsg = libc::CMSG_NXTHDR(&message, cmsg);
        }
    }
    Ok(None)
}

/// Arranges for `pairs.0` to appear as `pairs.1` in the child of `command`.
///
/// This is the descriptor-inheritance half of the re-exec: the listener has some
/// arbitrary fd number here and must be fd 3, 4, ... over there.
///
/// The hook itself runs after `fork` and before `exec`, in a process that may
/// share memory with other threads, so it only calls `dup2`/`fcntl` — both async
/// signal safe — and allocates nothing.
pub fn inherit_as(command: &mut Command, pairs: Vec<(RawFd, RawFd)>) {
    // SAFETY: the closure only calls async-signal-safe descriptor functions and
    // touches no memory that another thread could be mutating.
    unsafe {
        command.pre_exec(move || {
            for (from, to) in &pairs {
                if from == to {
                    // Already at the right number, but every socket we create is
                    // close-on-exec, so clear that or it disappears at exec.
                    cvt(libc::fcntl(*to, libc::F_SETFD, 0))?;
                } else {
                    // dup2 clears close-on-exec on the target.
                    cvt(libc::dup2(*from, *to))?;
                }
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;

    #[test]
    fn fd_passing_roundtrip() {
        let (left, right) = super::super::socket::socketpair().unwrap();
        let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(theirs);

        let mut byte = 0u8;
        send_with_fd(left.as_raw_fd(), b'X', mine.as_raw_fd()).unwrap();
        let received = recv_with_fd(right.as_raw_fd(), &mut byte).unwrap();

        assert_eq!(byte, b'X');
        let fd = received.expect("a descriptor");
        assert!(is_open(fd));
        close(fd);
        assert!(!is_open(fd));
    }

    #[test]
    fn adopted_sockets_are_close_on_exec() {
        let listener = super::super::socket::bind_tcp("127.0.0.1:0").unwrap();
        let fd = listener.as_raw_fd();
        std::mem::forget(listener);

        let adopted = adopt_listener(fd).unwrap();
        assert!(is_cloexec(adopted.as_raw_fd()));
        assert!(adopted.set_nonblocking(false).is_ok());
    }

    #[test]
    fn adopting_the_wrong_kind_of_descriptor_fails_cleanly() {
        let (left, _right) = super::super::socket::socketpair().unwrap();
        // A Unix socket is not a TCP listener, and a pipe is not a socket.
        assert!(adopt_listener(left.as_raw_fd()).is_err());
        assert!(adopt_udp_socket(left.as_raw_fd()).is_err());

        let (reader, _writer) = super::super::socket::pipe().unwrap();
        assert!(adopt_listener(reader.as_raw_fd()).is_err());
        assert!(adopt_unix_stream(reader.as_raw_fd()).is_err());
    }

    #[test]
    fn closed_descriptors_are_not_open() {
        assert!(!is_open(-1));
        assert!(!is_open(4093));
    }

    #[test]
    fn inherited_descriptors_survive_exec_at_the_expected_number() {
        // Write a known file, inherit it as fd 7, and read it back in a child
        // exactly the way the re-exec hands the listener to `__serve`.
        let path = std::env::temp_dir().join(format!("adhoc-inherit-{}", std::process::id()));
        std::fs::write(&path, b"inherited\n").unwrap();
        let file = std::fs::File::open(&path).unwrap();

        let mut command = Command::new("sh");
        command.args(["-c", "cat <&7"]);
        inherit_as(&mut command, vec![(file.as_raw_fd(), 7)]);

        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"inherited\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_passed_descriptor_can_be_read_over_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (left, right) = super::super::socket::socketpair().unwrap();

        std::thread::spawn(move || {
            let peer = TcpStream::connect(addr).unwrap();
            send_with_fd(left.as_raw_fd(), 1, peer.as_raw_fd()).unwrap();
            // Keep the peer open until the receiver read the data.
            std::thread::sleep(std::time::Duration::from_millis(200));
        });

        let mut byte = 0u8;
        let fd = recv_with_fd(right.as_raw_fd(), &mut byte).unwrap().unwrap();
        let mut peer = adopt_stream(fd).unwrap();
        let (mut server_side, _) = listener.accept().unwrap();
        server_side.write_all(b"hi").unwrap();
        let mut buffer = [0u8; 2];
        peer.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"hi");
    }
}
