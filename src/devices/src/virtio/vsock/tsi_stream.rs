use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::num::Wrapping;
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use socket2::{Domain, Socket, Type};

use super::super::Queue as VirtQueue;
use super::defs;
use super::defs::uapi;
use super::muxer::{MuxerRx, push_packet};
use super::muxer_rxq::MuxerRxQ;
use super::packet::{
    TsiAcceptReq, TsiConnectReq, TsiGetnameRsp, TsiListenReq, TsiSendtoAddr, VsockPacket,
};
use super::proxy::{
    Family, ListenerDesc, NewProxyType, Proxy, ProxyError, ProxyRemoval, ProxyStatus, ProxyUpdate,
    RecvPkt,
};
use super::sys;
use super::vsock_addr::VsockAddr;
use utils::epoll::EventSet;

use vm_memory::GuestMemoryMmap;

pub struct TsiStreamProxy {
    id: u64,
    cid: u64,
    parent_id: u64,
    family: Family,
    local_port: u32,
    peer_port: u32,
    control_port: u32,
    sock: Socket,
    pub status: ProxyStatus,
    mem: GuestMemoryMmap,
    queue: Arc<Mutex<VirtQueue>>,
    rxq: Arc<Mutex<MuxerRxQ>>,
    rx_cnt: Wrapping<u32>,
    tx_cnt: Wrapping<u32>,
    last_tx_cnt_sent: Wrapping<u32>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: Wrapping<u32>,
    push_cnt: Wrapping<u32>,
    pending_accepts: u64,
    #[cfg(target_os = "linux")]
    unixsock_path: Option<PathBuf>,
    /// Guest listen port, set once this proxy becomes a listener (via
    /// `try_listen`/`relisten`). Captured into a [`ListenerDesc`] so a fork clone
    /// can re-establish the host-side listener. 0 = not a listener.
    listen_guest_port: u16,
    /// Listen backlog requested by the guest, for the same reason.
    listen_backlog: i32,
}

/// Raw socket handle used for epoll registration (Unix file descriptor).
#[cfg(unix)]
fn raw_handle(sock: &Socket) -> RawFd {
    sock.as_raw_fd()
}

impl TsiStreamProxy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        cid: u64,
        family: u16,
        local_port: u32,
        peer_port: u32,
        control_port: u32,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
    ) -> Result<Self, ProxyError> {
        let (family, domain) = match family {
            defs::LINUX_AF_INET => (Family::Inet, Domain::IPV4),
            defs::LINUX_AF_INET6 => (Family::Inet6, Domain::IPV6),
            #[cfg(target_os = "linux")]
            defs::LINUX_AF_UNIX => (Family::Unix, Domain::UNIX),
            _ => return Err(ProxyError::InvalidFamily),
        };
        let sock = Socket::new(domain, Type::STREAM, None).map_err(ProxyError::CreatingSocket)?;
        if let Err(e) = sock.set_nonblocking(true) {
            warn!("error switching to non-blocking: id={id}, err={e}");
        }

        // SO_REUSEADDR mirrors the previous behavior (Unix used REUSEADDR, INET
        // used REUSEPORT; Windows has no REUSEPORT, so REUSEADDR is the portable
        // choice for re-binding a listener).
        let _ = sock.set_reuse_address(true);

        // Enable TCP keepalive to prevent silent drops on idle INET connections.
        #[cfg(target_os = "linux")]
        let is_unix = family == Family::Unix;
        #[cfg(not(target_os = "linux"))]
        let is_unix = false;
        if !is_unix {
            let ka = socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(60))
                .with_interval(std::time::Duration::from_secs(15));
            let _ = sock.set_tcp_keepalive(&ka);
        }

        Ok(TsiStreamProxy {
            id,
            cid,
            parent_id: 0,
            family,
            local_port,
            peer_port,
            control_port,
            sock,
            status: ProxyStatus::Idle,
            mem,
            queue,
            rxq,
            rx_cnt: Wrapping(0),
            tx_cnt: Wrapping(0),
            last_tx_cnt_sent: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            push_cnt: Wrapping(0),
            pending_accepts: 0,
            #[cfg(target_os = "linux")]
            unixsock_path: None,
            listen_guest_port: 0,
            listen_backlog: 0,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_reverse(
        id: u64,
        cid: u64,
        parent_id: u64,
        family: Family,
        local_port: u32,
        peer_port: u32,
        sock: Socket,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
    ) -> Self {
        debug!("new_reverse: id={id} local_port={local_port} peer_port={peer_port}");
        TsiStreamProxy {
            id,
            cid,
            parent_id,
            family,
            local_port,
            peer_port,
            control_port: 0,
            sock,
            status: ProxyStatus::ReverseInit,
            mem,
            queue,
            rxq,
            rx_cnt: Wrapping(0),
            tx_cnt: Wrapping(0),
            last_tx_cnt_sent: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            push_cnt: Wrapping(0),
            pending_accepts: 0,
            #[cfg(target_os = "linux")]
            unixsock_path: None,
            listen_guest_port: 0,
            listen_backlog: 0,
        }
    }

    fn init_data_pkt(&self, pkt: &mut VsockPacket) {
        debug!(
            "init_data_pkt: id={}, local_port={}, peer_port={}",
            self.id, self.local_port, self.peer_port
        );
        pkt.set_op(uapi::VSOCK_OP_RW)
            .set_src_cid(uapi::VSOCK_HOST_CID)
            .set_dst_cid(self.cid)
            .set_src_port(self.local_port)
            .set_dst_port(self.peer_port)
            .set_type(uapi::VSOCK_TYPE_STREAM)
            .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32)
            .set_fwd_cnt(self.tx_cnt.0);
    }

    fn try_listen(&mut self, req: &TsiListenReq, host_port_map: &Option<HashMap<u16, u16>>) -> i32 {
        if self.status == ProxyStatus::Listening || self.status == ProxyStatus::WaitingOnAccept {
            return 0;
        }

        // Resolve the bind address, applying the host port map for INET listeners.
        let mapped: Option<SocketAddr> = match req.addr.inet() {
            Some(inet) => {
                if let Some(port_map) = host_port_map {
                    match port_map.get(&inet.port()) {
                        Some(host_port) => Some(match inet {
                            SocketAddr::V4(a) => {
                                SocketAddr::V4(SocketAddrV4::new(*a.ip(), *host_port))
                            }
                            SocketAddr::V6(a) => SocketAddr::V6(SocketAddrV6::new(
                                *a.ip(),
                                *host_port,
                                a.flowinfo(),
                                a.scope_id(),
                            )),
                        }),
                        None => return -libc::EPERM,
                    }
                } else {
                    Some(inet)
                }
            }
            None => None, // AF_UNIX (Linux) handled below
        };

        // Remember the guest listen port (the host_port_map key) so this listener
        // can be re-established on a fork clone (see ListenerDesc).
        let guest_listen_port = req.addr.inet().map(|s| s.port()).unwrap_or(0);

        let bind_res: std::io::Result<()> = match mapped {
            Some(addr) => self.sock.bind(&addr.into()),
            #[cfg(target_os = "linux")]
            None => self.bind_unix(req),
            #[cfg(not(target_os = "linux"))]
            None => return -libc::EINVAL,
        };

        match bind_res {
            Ok(_) => {
                debug!("tcp bind: id={}", self.id);
                let clamped_backlog = req.backlog.clamp(0, libc::SOMAXCONN);
                match self.sock.listen(clamped_backlog) {
                    Ok(_) => {
                        debug!("proxy: id={}", self.id);
                        self.listen_guest_port = guest_listen_port;
                        self.listen_backlog = clamped_backlog;
                        0
                    }
                    Err(e) => {
                        warn!("proxy listen: id={} err={}", self.id, e);
                        -sys::to_linux_errno(&e)
                    }
                }
            }
            Err(e) => {
                warn!("tcp bind: id={} err={}", self.id, e);
                -sys::to_linux_errno(&e)
            }
        }
    }

    /// Bind an AF_UNIX listener (Linux only). Unlinks any stale socket node first
    /// so we can take ownership, and records the path for unlink-on-drop.
    #[cfg(target_os = "linux")]
    fn bind_unix(&mut self, req: &TsiListenReq) -> std::io::Result<()> {
        let path = req.addr.unix_path();
        if let Some(p) = &path
            && let Err(e) = fs::remove_file(p)
        {
            debug!("error removing socket: {e}");
        }
        let addr = match &path {
            Some(p) => socket2::SockAddr::unix(p)?,
            None => {
                return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
            }
        };
        self.sock.bind(&addr)?;
        // Track for unlink on Drop if it is a real on-disk socket path.
        if let Some(p) = path
            && fs::metadata(&p)
                .map(|m| {
                    use std::os::unix::fs::FileTypeExt;
                    m.file_type().is_socket()
                })
                .unwrap_or(false)
        {
            self.unixsock_path = Some(p);
        }
        Ok(())
    }

    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
    }

    fn recv_to_pkt(&self, pkt: &mut VsockPacket) -> RecvPkt {
        if let Some(buf) = pkt.buf_mut() {
            let peer_credit = self.peer_avail_credit();
            let max_len = std::cmp::min(buf.len(), peer_credit);

            debug!(
                "recv_to_pkt: peer_avail_credit={}, buf.len={}, max_len={}",
                self.peer_avail_credit(),
                buf.len(),
                max_len,
            );

            if max_len == 0 {
                return RecvPkt::WaitForCredit;
            }

            match sys::recv_into(&self.sock, &mut buf[..max_len]) {
                Ok(cnt) => {
                    debug!("recv cnt={cnt}");
                    if cnt > 0 {
                        debug!("recv rx_cnt={}", self.rx_cnt);
                        RecvPkt::Read(cnt)
                    } else {
                        RecvPkt::Close
                    }
                }
                Err(e) => {
                    debug!("recv_pkt: recv error: {e:?}");
                    RecvPkt::Error
                }
            }
        } else {
            debug!("recv_pkt: pkt without buf");
            RecvPkt::Error
        }
    }

    fn recv_pkt(&mut self) -> (bool, bool) {
        let mut have_used = false;
        let mut wait_credit = false;
        let mut queue = self.queue.lock().unwrap();

        while let Some(head) = queue.pop(&self.mem) {
            let len = match VsockPacket::from_rx_virtq_head(&head) {
                Ok(mut pkt) => match self.recv_to_pkt(&mut pkt) {
                    RecvPkt::WaitForCredit => {
                        wait_credit = true;
                        0
                    }
                    RecvPkt::Read(cnt) => {
                        self.rx_cnt += Wrapping(cnt as u32);
                        self.init_data_pkt(&mut pkt);
                        pkt.set_len(cnt as u32);
                        pkt.hdr().len() + cnt
                    }
                    RecvPkt::Close => {
                        self.status = ProxyStatus::PeerClosed;
                        0
                    }
                    RecvPkt::Error => 0,
                },
                Err(e) => {
                    debug!("recv_pkt: RX queue error: {e:?}");
                    0
                }
            };

            if len == 0 {
                queue.undo_pop();
                break;
            } else {
                have_used = true;
                self.push_cnt += Wrapping(len as u32);
                debug!(
                    "recv_pkt: pushing packet with {} bytes, push_cnt={}",
                    len, self.push_cnt
                );
                if let Err(e) = queue.add_used(&self.mem, head.index, len as u32) {
                    error!("failed to add used elements to the queue: {e:?}");
                }
            }
        }

        debug!("recv_pkt: have_used={have_used}");
        (have_used, wait_credit)
    }

    fn push_connect_rsp(&self, result: i32) {
        debug!(
            "push_connect_rsp: id: {}, control_port: {}, result: {}",
            self.id, self.control_port, result
        );

        // This response goes to the control port (DGRAM).
        let rx = MuxerRx::ConnResponse {
            local_port: 1025,
            peer_port: self.control_port,
            result,
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    fn push_reset(&self) {
        debug!(
            "push_reset: id: {}, peer_port: {}, local_port: {}",
            self.id, self.peer_port, self.local_port
        );

        // This response goes to the connection.
        let rx = MuxerRx::Reset {
            local_port: self.local_port,
            peer_port: self.peer_port,
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    fn switch_to_connected(&mut self) {
        self.status = ProxyStatus::Connected;
        if let Err(e) = self.sock.set_nonblocking(false) {
            warn!("error switching to blocking: id={}, err={}", self.id, e);
        }
    }

    /// Re-establish this freshly-constructed proxy as a host-side inbound
    /// listener for a fork clone, using the clone's own `host_port_map` (see
    /// [`ListenerDesc`]). Mirrors `try_listen`'s bind+listen but is driven by the
    /// snapshot rather than a guest `TSI_LISTEN`. Returns 0 on success, a
    /// negative errno otherwise.
    pub fn relisten(
        &mut self,
        guest_port: u16,
        backlog: i32,
        host_port_map: &Option<HashMap<u16, u16>>,
    ) -> i32 {
        let host_port = match host_port_map.as_ref().and_then(|m| m.get(&guest_port)) {
            Some(p) => *p,
            None => return -libc::EPERM,
        };
        let addr: SocketAddr = match self.family {
            Family::Inet => SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, host_port).into(),
            Family::Inet6 => {
                SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, host_port, 0, 0).into()
            }
            #[cfg(unix)]
            Family::Unix => return -libc::EINVAL,
        };
        if let Err(e) = self.sock.bind(&addr.into()) {
            warn!("relisten bind id={} err={e}", self.id);
            return -libc::EADDRINUSE;
        }
        let clamped = backlog.clamp(0, libc::SOMAXCONN);
        if let Err(e) = self.sock.listen(clamped) {
            warn!("relisten listen id={} err={e}", self.id);
            return -libc::EINVAL;
        }
        self.status = ProxyStatus::Listening;
        self.listen_guest_port = guest_port;
        self.listen_backlog = clamped;
        debug!(
            "relisten: id={} guest_port={} host_port={}",
            self.id, guest_port, host_port
        );
        0
    }
}

impl Proxy for TsiStreamProxy {
    fn id(&self) -> u64 {
        self.id
    }

    fn status(&self) -> ProxyStatus {
        self.status
    }

    fn listener_desc(&self) -> Option<ListenerDesc> {
        if (self.status == ProxyStatus::Listening || self.status == ProxyStatus::WaitingOnAccept)
            && self.listen_guest_port != 0
        {
            let family = match self.family {
                Family::Inet => defs::LINUX_AF_INET,
                Family::Inet6 => defs::LINUX_AF_INET6,
                #[cfg(unix)]
                Family::Unix => return None,
            };
            Some(ListenerDesc {
                family,
                peer_port: self.peer_port,
                control_port: self.control_port,
                guest_port: self.listen_guest_port,
                backlog: self.listen_backlog,
            })
        } else {
            None
        }
    }

    fn connect(&mut self, _pkt: &VsockPacket, req: TsiConnectReq) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        let connect_addr: std::io::Result<socket2::SockAddr> = match req.addr.inet() {
            Some(inet) => Ok(inet.into()),
            #[cfg(target_os = "linux")]
            None => match req.addr.unix_path() {
                Some(p) => socket2::SockAddr::unix(p),
                None => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
            },
            #[cfg(not(target_os = "linux"))]
            None => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
        };

        let result = match connect_addr {
            Ok(addr) => match self.sock.connect(&addr) {
                Ok(()) => {
                    debug!("connect: Connected");
                    self.switch_to_connected();
                    0
                }
                Err(e) if sys::connect_in_progress(&e) => {
                    debug!("connect: Connecting");
                    self.status = ProxyStatus::Connecting;
                    0
                }
                Err(e) => {
                    debug!("TcpProxy: Error connecting: {e}");
                    -sys::to_linux_errno(&e)
                }
            },
            Err(e) => -sys::to_linux_errno(&e),
        };

        if self.status == ProxyStatus::Connecting {
            update.polling = Some((
                self.id,
                raw_handle(&self.sock),
                EventSet::OUT | EventSet::EDGE_TRIGGERED,
            ));
        } else {
            if self.status == ProxyStatus::Connected {
                update.polling = Some((self.id, raw_handle(&self.sock), EventSet::IN));
            }
            self.push_connect_rsp(result);
        }

        update
    }

    fn confirm_connect(&mut self, pkt: &VsockPacket) -> Option<ProxyUpdate> {
        debug!(
            "confirm_connect: local_port={} peer_port={}, src_port={}, dst_port={}",
            pkt.dst_port(),
            pkt.src_port(),
            self.local_port,
            self.peer_port,
        );

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        self.local_port = pkt.dst_port();
        self.peer_port = pkt.src_port();

        // This response goes to the connection.
        let rx = MuxerRx::OpResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);

        // Now that the vsock transport is fully established, start listening
        // for events in the TCP socket again.
        Some(ProxyUpdate {
            polling: Some((self.id, raw_handle(&self.sock), EventSet::IN)),
            ..Default::default()
        })
    }

    fn getpeername(&mut self, pkt: &VsockPacket) {
        debug!("getpeername: id={}", self.id);

        let (result, addr_len, addr): (i32, u32, VsockAddr) = match self.sock.peer_addr() {
            Ok(sa) => match sa.as_socket() {
                Some(socket_addr) => {
                    let va = VsockAddr::Inet(socket_addr);
                    let len = va.linux_len();
                    (0, len, va)
                }
                None => (
                    -libc::EINVAL,
                    0,
                    VsockAddr::Inet(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()),
                ),
            },
            Err(e) => (
                -sys::to_linux_errno(&e),
                0,
                VsockAddr::Inet(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()),
            ),
        };

        let data = TsiGetnameRsp {
            result,
            addr_len,
            addr,
        };

        debug!("getpeername: reply={data:?}");

        // This response goes to the control port (DGRAM).
        let rx = MuxerRx::GetnameResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
            data,
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    fn sendmsg(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        debug!("sendmsg");

        // If the proxy is already closed (e.g. remote server closed the connection
        // while the guest was idle), send RST immediately so the guest gets a clean
        // error instead of writing into the void and timing out.
        if self.status == ProxyStatus::Closed || self.status == ProxyStatus::PeerClosed {
            debug!(
                "sendmsg: proxy closed/peer-closed, sending RST: id={}",
                self.id
            );
            self.push_reset();
            return ProxyUpdate {
                signal_queue: true,
                ..Default::default()
            };
        }

        let mut update = ProxyUpdate::default();

        let ret = if let Some(buf) = pkt.buf() {
            // MSG_NOSIGNAL on Linux avoids SIGPIPE on a closed peer; macOS relies
            // on the socket's SO_NOSIGPIPE / default, Windows has no SIGPIPE.
            #[cfg(target_os = "linux")]
            let send_res = self.sock.send_with_flags(buf, libc::MSG_NOSIGNAL);
            #[cfg(not(target_os = "linux"))]
            let send_res = self.sock.send(buf);

            match send_res {
                Ok(sent) => {
                    if sent != buf.len() {
                        error!("couldn't set everything: buf={}, sent={}", buf.len(), sent);
                    }
                    self.tx_cnt += Wrapping(sent as u32);
                    sent as i32
                }
                Err(err) => -sys::to_linux_errno(&err),
            }
        } else {
            -libc::EINVAL
        };

        if ret > 0 && (self.tx_cnt - self.last_tx_cnt_sent).0 >= self.peer_buf_alloc / 2 {
            debug!(
                "sending credit update: id={}, tx_cnt={}, last_tx_cnt={}",
                self.id, self.tx_cnt, self.last_tx_cnt_sent
            );
            self.last_tx_cnt_sent = self.tx_cnt;
            // This packet goes to the connection.
            let rx = MuxerRx::CreditUpdate {
                local_port: pkt.dst_port(),
                peer_port: pkt.src_port(),
                fwd_cnt: self.tx_cnt.0,
            };
            push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
            update.signal_queue = true;
        }

        debug!("sendmsg ret={ret}");
        update
    }

    fn sendto_addr(&mut self, _req: TsiSendtoAddr) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn listen(
        &mut self,
        pkt: &VsockPacket,
        req: TsiListenReq,
        host_port_map: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        debug!(
            "listen: id={} vm_port={} backlog={}",
            self.id, req.vm_port, req.backlog
        );
        let mut update = ProxyUpdate::default();

        let result = self.try_listen(&req, host_port_map);

        // This packet goes to the control port (DGRAM).
        let rx = MuxerRx::ListenResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
            result,
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);

        if result == 0 {
            self.peer_port = req.vm_port;
            self.status = ProxyStatus::Listening;
            update.polling = Some((self.id, raw_handle(&self.sock), EventSet::IN));
        }

        update
    }

    fn accept(&mut self, req: TsiAcceptReq) -> ProxyUpdate {
        debug!("accept: id={} flags={}", req.peer_port, req.flags);

        let mut update = ProxyUpdate::default();

        if self.pending_accepts > 0 {
            self.pending_accepts -= 1;
            self.push_accept_rsp(0);
            update.signal_queue = true;
        } else if (req.flags & libc::O_NONBLOCK as u32) != 0 {
            self.push_accept_rsp(-libc::EWOULDBLOCK);
            update.signal_queue = true;
        } else {
            self.status = ProxyStatus::WaitingOnAccept;
        }

        update
    }

    fn update_peer_credit(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        debug!(
            "update_credit: buf_alloc={} rx_cnt={} fwd_cnt={}",
            pkt.buf_alloc(),
            self.rx_cnt,
            pkt.fwd_cnt()
        );
        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        self.status = ProxyStatus::Connected;

        ProxyUpdate {
            polling: Some((self.id, raw_handle(&self.sock), EventSet::IN)),
            ..Default::default()
        }
    }

    fn push_op_request(&self) {
        debug!(
            "push_op_request: id={}, local_port={} peer_port={}",
            self.id, self.local_port, self.peer_port
        );

        // This packet goes to the connection.
        let rx = MuxerRx::OpRequest {
            local_port: self.local_port,
            peer_port: self.peer_port,
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    fn process_op_response(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        debug!(
            "process_op_response: id={} src_port={} dst_port={}",
            self.id,
            pkt.src_port(),
            pkt.dst_port()
        );

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        self.switch_to_connected();

        ProxyUpdate {
            polling: Some((self.id, raw_handle(&self.sock), EventSet::IN)),
            push_accept: Some((self.id, self.parent_id)),
            ..Default::default()
        }
    }

    fn enqueue_accept(&mut self) {
        debug!("enqueue_accept: control_port: {}", self.control_port);

        if self.status == ProxyStatus::WaitingOnAccept {
            self.status = ProxyStatus::Listening;
            self.push_accept_rsp(0);
        } else {
            self.pending_accepts += 1;
        }
    }

    fn push_accept_rsp(&self, result: i32) {
        debug!(
            "push_accept_rsp: control_port: {}, result: {}",
            self.control_port, result
        );

        // This packet goes to the control port (DGRAM).
        let rx = MuxerRx::AcceptResponse {
            local_port: 1030,
            peer_port: self.control_port,
            result,
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    fn shutdown(&mut self, pkt: &VsockPacket) {
        let recv_off = pkt.flags() & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0;
        let send_off = pkt.flags() & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0;

        let how = if recv_off && send_off {
            std::net::Shutdown::Both
        } else if recv_off {
            std::net::Shutdown::Read
        } else {
            std::net::Shutdown::Write
        };

        if let Err(e) = self.sock.shutdown(how) {
            warn!("error sending shutdown to socket: {e}");
        }
    }

    fn release(&mut self) -> ProxyUpdate {
        debug!(
            "release: id={}, tx_cnt={}, last_tx_cnt={}",
            self.id, self.tx_cnt, self.last_tx_cnt_sent
        );
        let remove_proxy = if self.status == ProxyStatus::Listening {
            ProxyRemoval::Immediate
        } else {
            ProxyRemoval::Deferred
        };
        ProxyUpdate {
            remove_proxy,
            ..Default::default()
        }
    }

    fn process_event(&mut self, evset: EventSet) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        if evset.contains(EventSet::HANG_UP) {
            debug!("process_event: HANG_UP");
            if self.status == ProxyStatus::Connecting {
                self.push_connect_rsp(-libc::ECONNREFUSED);
                self.status = ProxyStatus::Closed;
                update.polling = Some((self.id, raw_handle(&self.sock), EventSet::empty()));
                update.signal_queue = true;
                update.remove_proxy = ProxyRemoval::Deferred;
                return update;
            } else if self.status == ProxyStatus::Connected {
                // Drain any remaining data before signaling closure.
                let (signal_queue, _) = self.recv_pkt();
                update.signal_queue = signal_queue;
                // Send RST to force-close the vsock connection (see git history
                // for why RST and not SHUTDOWN).
                self.push_reset();
                self.status = ProxyStatus::Closed;
                update.signal_queue = true;
                update.polling = Some((self.id, raw_handle(&self.sock), EventSet::empty()));
                update.remove_proxy = ProxyRemoval::Deferred;
                return update;
            } else {
                self.push_reset();
                self.status = ProxyStatus::Closed;
                update.polling = Some((self.id, raw_handle(&self.sock), EventSet::empty()));
                update.signal_queue = true;
                update.remove_proxy = if self.status == ProxyStatus::Listening {
                    ProxyRemoval::Immediate
                } else {
                    ProxyRemoval::Deferred
                };
                return update;
            }
        }

        if evset.contains(EventSet::IN) {
            debug!("process_event: IN");
            if self.status == ProxyStatus::Connected {
                let (signal_queue, wait_credit) = self.recv_pkt();
                update.signal_queue = signal_queue;

                if wait_credit && self.status != ProxyStatus::WaitingCreditUpdate {
                    self.status = ProxyStatus::WaitingCreditUpdate;
                    let rx = MuxerRx::CreditRequest {
                        local_port: self.local_port,
                        peer_port: self.peer_port,
                        fwd_cnt: self.tx_cnt.0,
                    };
                    update.push_credit_req = Some(rx);
                }

                if self.status == ProxyStatus::PeerClosed {
                    debug!("process_event: peer closed, sending reset: id={}", self.id);
                    self.push_reset();
                    self.status = ProxyStatus::Closed;
                    update.signal_queue = true;
                    update.polling = Some((self.id(), raw_handle(&self.sock), EventSet::empty()));
                    update.remove_proxy = ProxyRemoval::Deferred;
                    return update;
                } else if self.status == ProxyStatus::WaitingCreditUpdate {
                    debug!("process_event: WaitingCreditUpdate");
                    update.polling = Some((self.id(), raw_handle(&self.sock), EventSet::empty()));
                }
            } else if self.status == ProxyStatus::Listening
                || self.status == ProxyStatus::WaitingOnAccept
            {
                match self.sock.accept() {
                    Ok((new_sock, _addr)) => {
                        // Use the original vsock ephemeral port (encoded in the
                        // high 32 bits of self.id) rather than self.peer_port,
                        // which listen() overwrites with the VM TCP port.
                        let vsock_peer_port = (self.id >> 32) as u32;
                        update.new_proxy =
                            Some((vsock_peer_port, new_sock, self.family, NewProxyType::Tcp));
                    }
                    Err(e) => warn!("error accepting connection: id={}, err={}", self.id, e),
                };
                update.signal_queue = true;
                return update;
            } else {
                debug!("EventSet::IN while not connected: {:?}", self.status);
            }
        }

        if evset.contains(EventSet::OUT) {
            debug!("process_event: OUT");
            if self.status == ProxyStatus::Connecting {
                self.switch_to_connected();
                self.push_connect_rsp(0);
                update.signal_queue = true;
                // Stop listening for events in the TCP socket until we receive
                // OP_REQUEST and the vsock transport is fully established.
                update.polling = Some((self.id(), raw_handle(&self.sock), EventSet::empty()));
            } else {
                debug!("EventSet::OUT while not connecting");
            }
        }

        update
    }
}

#[cfg(unix)]
impl AsRawFd for TsiStreamProxy {
    fn as_raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

#[cfg(target_os = "linux")]
impl Drop for TsiStreamProxy {
    fn drop(&mut self) {
        if let Some(path) = &self.unixsock_path {
            _ = fs::remove_file(path);
        }
    }
}
