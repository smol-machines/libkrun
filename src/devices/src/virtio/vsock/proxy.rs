use std::collections::HashMap;
use std::fmt;
use std::io;
#[cfg(unix)]
use std::os::unix::io::RawFd;

use socket2::Socket;

use super::muxer::MuxerRx;
use super::packet::{TsiAcceptReq, TsiConnectReq, TsiListenReq, TsiSendtoAddr, VsockPacket};
use serde::{Deserialize, Serialize};
use utils::epoll::EventSet;

/// Raw handle a proxy hands to the epoll layer for readiness registration. On
/// Unix this is the socket's file descriptor; on Windows it is the Winsock
/// SOCKET cast to the epoll shim's `HANDLE`-shaped `RawFd` (the shim detects
/// sockets and bridges them via `WSAEventSelect`).
#[cfg(unix)]
pub type ProxyRawHandle = RawFd;
#[cfg(windows)]
pub type ProxyRawHandle = utils::windows::RawFd;

/// Compute the [`ProxyRawHandle`] for a socket2 socket, cross-platform.
#[cfg(unix)]
pub fn raw_handle(sock: &Socket) -> ProxyRawHandle {
    use std::os::unix::io::AsRawFd;
    sock.as_raw_fd()
}
#[cfg(windows)]
pub fn raw_handle(sock: &Socket) -> ProxyRawHandle {
    use std::os::windows::io::AsRawSocket;
    sock.as_raw_socket() as ProxyRawHandle
}

/// Address family of a TSI proxy, replacing the Unix-only `nix::AddressFamily`.
/// AF_UNIX is Linux-only (macOS/Windows do not proxy AF_UNIX over TSI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Inet,
    Inet6,
    /// AF_UNIX: available on Unix hosts (used by the host-IPC `UnixProxy`); not
    /// on Windows.
    #[cfg(unix)]
    Unix,
}

/// Snapshot of a live TSI inbound-port-forward listener, captured so a
/// cross-process fork clone can re-establish it. The guest's `listen()` is
/// intercepted by TSI and only ever issued once, so a snapshot-restored clone
/// (whose app is already past `listen()`) never re-arms the host-side listener —
/// the muxer's `proxy_map` is not part of the device snapshot. On restore the
/// muxer rebuilds each listener from this descriptor using the *clone's* own
/// host port map, so its remapped inbound port works. `peer_port`/`control_port`
/// are the guest socket's TSI ports (preserved verbatim so host-accepted
/// connections route to the guest's still-listening, CoW-restored socket).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerDesc {
    /// Address family as a Linux AF_* value (`defs::LINUX_AF_INET`/`INET6`).
    pub family: u16,
    /// Guest socket's TSI port (the `id` high word).
    pub peer_port: u32,
    /// Guest control port for this proxy (`pkt.src_port()` at create time).
    pub control_port: u32,
    /// Guest listen port — the key into the host port map.
    pub guest_port: u16,
    /// Listen backlog the guest requested.
    pub backlog: i32,
}

#[derive(Debug)]
pub enum RecvPkt {
    Close,
    Error,
    Read(usize),
    WaitForCredit,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum ProxyError {
    CreatingSocket(io::Error),
    InvalidFamily,
}

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum ProxyStatus {
    Idle,
    Connecting,
    Connected,
    Listening,
    Closed,
    PeerClosed,
    WaitingCreditUpdate,
    ReverseInit,
    WaitingOnAccept,
}

#[derive(Default)]
pub enum ProxyRemoval {
    #[default]
    Keep,
    Immediate,
    Deferred,
}

#[derive(Default)]
pub enum NewProxyType {
    #[default]
    Tcp,
    #[cfg(unix)]
    Unix,
}

#[derive(Default)]
pub struct ProxyUpdate {
    pub signal_queue: bool,
    pub remove_proxy: ProxyRemoval,
    pub polling: Option<(u64, ProxyRawHandle, EventSet)>,
    pub new_proxy: Option<(u32, Socket, Family, NewProxyType)>,
    pub push_accept: Option<(u64, u64)>,
    pub push_credit_req: Option<MuxerRx>,
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

pub trait Proxy: Send {
    /// Raw handle the muxer registers with the epoll layer for this proxy's host
    /// socket. Cross-platform replacement for the former `AsRawFd` supertrait.
    fn poll_handle(&self) -> ProxyRawHandle;
    fn id(&self) -> u64;
    #[allow(dead_code)]
    fn status(&self) -> ProxyStatus;
    /// Whether this proxy is a UDP/datagram proxy. The muxer uses this to gate
    /// DNS interception to datagram sockets only.
    fn is_dgram(&self) -> bool {
        false
    }
    fn connect(&mut self, pkt: &VsockPacket, req: TsiConnectReq) -> ProxyUpdate;
    fn confirm_connect(&mut self, _pkt: &VsockPacket) -> Option<ProxyUpdate> {
        None
    }
    fn getpeername(&mut self, pkt: &VsockPacket);
    fn sendmsg(&mut self, pkt: &VsockPacket) -> ProxyUpdate;
    fn sendto_addr(&mut self, req: TsiSendtoAddr) -> ProxyUpdate;
    fn sendto_data(&mut self, _pkt: &VsockPacket) {}
    fn listen(
        &mut self,
        pkt: &VsockPacket,
        req: TsiListenReq,
        host_port_map: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate;
    fn accept(&mut self, req: TsiAcceptReq) -> ProxyUpdate;
    fn update_peer_credit(&mut self, pkt: &VsockPacket) -> ProxyUpdate;
    fn push_op_request(&self) {}
    fn process_op_response(&mut self, pkt: &VsockPacket) -> ProxyUpdate;
    fn enqueue_accept(&mut self) {}
    fn push_accept_rsp(&self, _result: i32) {}
    fn shutdown(&mut self, _pkt: &VsockPacket) {}
    fn release(&mut self) -> ProxyUpdate;
    fn process_event(&mut self, evset: EventSet) -> ProxyUpdate;
    /// True if this proxy is a passive listener/acceptor (e.g. a host unix-IPC
    /// socket awaiting inbound connections), as opposed to a live connection.
    /// Listeners are preserved across a VM restore so the muxer keeps accepting
    /// new connections; active connection proxies are dropped and re-established
    /// (see [`super::muxer::VsockMuxer::reset_connections`]).
    fn is_listener(&self) -> bool {
        false
    }
    /// If this proxy is a live TSI inbound-port-forward listener, return its
    /// descriptor so it can be snapshotted and re-established on a fork clone
    /// (see [`ListenerDesc`]). `None` for everything else (connections, the
    /// agent unix acceptor, datagram proxies).
    fn listener_desc(&self) -> Option<ListenerDesc> {
        None
    }
}
