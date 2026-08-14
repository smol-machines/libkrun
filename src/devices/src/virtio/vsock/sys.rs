// Copyright 2026 The libkrun Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Small cross-platform host-socket helpers for the vsock/TSI proxies.
//!
//! The proxies use `socket2::Socket` directly for most operations; these helpers
//! cover the few spots where the raw API is awkward or platform-divergent:
//!  * `recv_into` adapts `socket2`'s `MaybeUninit` recv to a `&mut [u8]` buffer,
//!  * `connect_in_progress` classifies the nonblocking-connect "in progress"
//!    error, which is `EINPROGRESS` on Unix but `WSAEWOULDBLOCK` on Windows,
//!  * `domain_of` picks the `socket2` domain for an address.
//!
//! See WINDOWS_NETWORKING_PORT.md.

use std::io;
use std::mem::MaybeUninit;

use socket2::Socket;

/// Listen backlog clamp, matching the platform's `SOMAXCONN`.
#[cfg(unix)]
pub const SOMAXCONN: i32 = libc::SOMAXCONN;
#[cfg(windows)]
pub const SOMAXCONN: i32 = 0x7fff_ffff;

/// Receive into an initialized byte slice, returning the number of bytes read.
///
/// `socket2::Socket::recv` takes `&mut [MaybeUninit<u8>]`; viewing an already
/// initialized `&mut [u8]` as uninitialized memory is sound (we never read the
/// "uninit" view, only write through it), and the returned count bounds what the
/// caller reads back.
///
/// On Unix the recv is forced non-blocking with `MSG_DONTWAIT`. The host-IPC
/// `UnixProxy` keeps its connected socket in *blocking* mode (so partial-write
/// `send`s drain fully), but its recv is driven by edge-readiness from the
/// muxer's epoll loop and must never block that single thread on a spurious or
/// already-drained `IN` wakeup — which a blocking `recv()` would do, hanging the
/// whole muxer. This restores the pre-socket2 `nix` behavior, which always
/// passed `MSG_DONTWAIT` regardless of the socket's blocking mode. Windows has
/// no `MSG_DONTWAIT`; there the socket's own non-blocking state governs recv,
/// matching the established (verified) Windows path.
pub fn recv_into(sock: &Socket, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `[u8]` and `[MaybeUninit<u8>]` have identical layout; we only hand
    // the slice to the kernel to write into and use the returned length.
    let uninit = unsafe {
        std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut MaybeUninit<u8>, buf.len())
    };
    #[cfg(unix)]
    {
        sock.recv_with_flags(uninit, libc::MSG_DONTWAIT)
    }
    #[cfg(windows)]
    {
        sock.recv(uninit)
    }
}

/// True if a nonblocking `connect` error means "connection in progress" rather
/// than a hard failure (`EINPROGRESS` on Unix, `WSAEWOULDBLOCK` on Windows).
pub fn connect_in_progress(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EINPROGRESS)
    }
    #[cfg(windows)]
    {
        // WSAEWOULDBLOCK
        e.raw_os_error() == Some(10035) || e.kind() == io::ErrorKind::WouldBlock
    }
}

/// Send the whole buffer, waiting for the peer as needed, regardless of the
/// socket's blocking state. Returns `buf.len()` on success.
///
/// The stream proxies' `sendmsg` was written for blocking sockets — the
/// proxies switch their socket to blocking after connect precisely so `send`
/// drains fully and its return is the flow control. On Windows that switch
/// silently fails: a socket registered with the muxer's epoll shim is under
/// `WSAEventSelect`, which forces nonblocking mode and makes `FIONBIO` refuse
/// to undo it while the registration exists. `send` can then accept a prefix
/// of the buffer or refuse outright with `WSAEWOULDBLOCK`, and treating
/// either as delivery silently drops bytes from the middle of a stream: a
/// bulk agent frame loses its tail so the host reader waits on bytes that
/// never arrive (surfacing as `WSAETIMEDOUT`), and a TSI download truncates
/// mid-body ("unexpected EOF"). Small request/response traffic never fills
/// the socket buffer, which is why only bulk transfers failed.
///
/// This emulates the blocking send the callers assume: resume partial sends
/// from the right offset, and on `WouldBlock` wait for writability before
/// retrying. On Unix the socket really is blocking, so the loop collapses to
/// the single `send` it always was.
pub fn send_all(sock: &Socket, buf: &[u8]) -> io::Result<usize> {
    let mut sent = 0;
    while sent < buf.len() {
        // MSG_NOSIGNAL on Linux avoids SIGPIPE on a closed peer; macOS relies
        // on the socket's SO_NOSIGPIPE / default, Windows has no SIGPIPE.
        #[cfg(target_os = "linux")]
        let res = sock.send_with_flags(&buf[sent..], libc::MSG_NOSIGNAL);
        #[cfg(not(target_os = "linux"))]
        let res = sock.send(&buf[sent..]);
        match res {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "peer accepted no bytes",
                ));
            }
            Ok(n) => sent += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(10035) => {
                wait_writable(sock)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(sent)
}

/// Block until `sock` is writable. The unbounded wait mirrors the blocking
/// `send` this stands in for; the peer closing surfaces as an error on the
/// next `send`, exactly as it would have mid-blocking-send.
#[cfg(unix)]
fn wait_writable(sock: &Socket) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let mut pfd = libc::pollfd {
        fd: sock.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc > 0 {
            return Ok(());
        }
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// Block until `sock` is writable (Windows).
///
/// `WSAPoll` is level-triggered, so it reports writability from the socket's
/// current buffer state and does not depend on the `FD_WRITE` edge that
/// `WSAEventSelect` consumes — safe to mix with the epoll shim's registration.
#[cfg(windows)]
fn wait_writable(sock: &Socket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        POLLWRNORM, WSAGetLastError, WSAPOLLFD, WSAPoll,
    };
    let mut pfd = WSAPOLLFD {
        fd: sock.as_raw_socket() as usize,
        events: POLLWRNORM,
        revents: 0,
    };
    loop {
        let rc = unsafe { WSAPoll(&mut pfd, 1, -1) };
        if rc > 0 {
            return Ok(());
        }
        if rc < 0 {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        // rc == 0 cannot happen with an infinite timeout; loop defensively.
    }
}

/// Translate a host socket error into the **Linux** errno the guest expects.
/// Identity on Linux; via the existing BSD→Linux table on macOS; via a Winsock
/// (`WSAE*`) → Linux mapping on Windows.
pub fn to_linux_errno(e: &io::Error) -> i32 {
    let raw = e.raw_os_error().unwrap_or(libc::EIO);
    #[cfg(target_os = "linux")]
    {
        raw
    }
    #[cfg(target_os = "macos")]
    {
        super::super::linux_errno::linux_errno_raw(raw)
    }
    #[cfg(target_os = "windows")]
    {
        wsa_to_linux_errno(raw)
    }
}

/// Map the common Winsock error codes to their Linux errno equivalents so the
/// guest kernel sees the errors it expects. Unknown codes fall back to EIO.
#[cfg(target_os = "windows")]
fn wsa_to_linux_errno(wsa: i32) -> i32 {
    // Linux errno values (stable on the guest ABI).
    const E_ACCES: i32 = 13;
    const E_INVAL: i32 = 22;
    const E_AGAIN: i32 = 11;
    const E_INPROGRESS: i32 = 115;
    const E_ADDRINUSE: i32 = 98;
    const E_ADDRNOTAVAIL: i32 = 99;
    const E_NETUNREACH: i32 = 101;
    const E_CONNABORTED: i32 = 103;
    const E_CONNRESET: i32 = 104;
    const E_NOTCONN: i32 = 107;
    const E_TIMEDOUT: i32 = 110;
    const E_CONNREFUSED: i32 = 111;
    const E_HOSTUNREACH: i32 = 113;
    const E_IO: i32 = 5;
    match wsa {
        10013 => E_ACCES,               // WSAEACCES
        10022 => E_INVAL,               // WSAEINVAL
        10035 => E_AGAIN,               // WSAEWOULDBLOCK
        10036 => E_INPROGRESS,          // WSAEINPROGRESS
        10037 => E_AGAIN,               // WSAEALREADY
        10048 => E_ADDRINUSE,           // WSAEADDRINUSE
        10049 => E_ADDRNOTAVAIL,        // WSAEADDRNOTAVAIL
        10050 | 10051 => E_NETUNREACH,  // WSAENETDOWN / WSAENETUNREACH
        10053 => E_CONNABORTED,         // WSAECONNABORTED
        10054 => E_CONNRESET,           // WSAECONNRESET
        10057 => E_NOTCONN,             // WSAENOTCONN
        10060 => E_TIMEDOUT,            // WSAETIMEDOUT
        10061 => E_CONNREFUSED,         // WSAECONNREFUSED
        10064 | 10065 => E_HOSTUNREACH, // WSAEHOSTDOWN / WSAEHOSTUNREACH
        _ => E_IO,
    }
}
