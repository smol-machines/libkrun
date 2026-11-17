use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::num::Wrapping;
use std::sync::{Arc, Mutex};

use socket2::{Domain, Socket, Type};

use super::super::Queue as VirtQueue;
use super::super::linux_errno::LINUX_EAFNOSUPPORT;
use super::defs;
use super::defs::uapi;
use super::dns_filter::{DNS_PORT, DnsRequest, sockaddr_port};
use super::muxer::{MuxerRx, push_packet};
use super::muxer_rxq::MuxerRxQ;
use super::packet::{
    TsiAcceptReq, TsiConnectReq, TsiGetnameRsp, TsiListenReq, TsiSendtoAddr, VsockPacket,
};
use super::proxy::{
    Family, Proxy, ProxyError, ProxyRawHandle, ProxyRemoval, ProxyStatus, ProxyUpdate, RecvPkt,
    raw_handle,
};
use super::snapshot_gate::SnapshotGate;
use super::sys;
use super::vsock_addr::VsockAddr;
use crossbeam_channel::Sender;
use utils::epoll::EventSet;

use vm_memory::GuestMemoryMmap;

pub struct TsiDgramProxy {
    pub id: u64,
    cid: u64,
    local_port: u32,
    peer_port: u32,
    sock: Socket,
    pub status: ProxyStatus,
    sendto_addr: Option<VsockAddr>,
    listening: bool,
    family: Family,
    mem: GuestMemoryMmap,
    queue: Arc<Mutex<VirtQueue>>,
    rxq: Arc<Mutex<MuxerRxQ>>,
    snapshot_gate: Arc<SnapshotGate>,
    rx_cnt: Wrapping<u32>,
    tx_cnt: Wrapping<u32>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: Wrapping<u32>,
    /// Sender to the DNS worker; `Some` only when DNS filtering is active.
    dns_sender: Option<Sender<DnsRequest>>,
    /// Set when this socket was `connect()`ed to a resolver (port 53) and is
    /// being intercepted — sends on it are routed to the DNS worker.
    connected_dns: bool,
}

impl TsiDgramProxy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        cid: u64,
        family: u16,
        peer_port: u32,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
        dns_sender: Option<Sender<DnsRequest>>,
        snapshot_gate: Arc<SnapshotGate>,
    ) -> Result<Self, ProxyError> {
        let (family, domain) = match family {
            defs::LINUX_AF_INET => (Family::Inet, Domain::IPV4),
            defs::LINUX_AF_INET6 => (Family::Inet6, Domain::IPV6),
            #[cfg(target_os = "linux")]
            defs::LINUX_AF_UNIX => (Family::Unix, Domain::UNIX),
            _ => return Err(ProxyError::InvalidFamily),
        };

        let sock = Socket::new(domain, Type::DGRAM, None).map_err(ProxyError::CreatingSocket)?;
        if let Err(e) = sock.set_nonblocking(true) {
            warn!("error switching to non-blocking: id={id}, err={e}");
        }
        // Match Linux's dual-stack default for IPv6 wildcard binds (no-op on IPv4).
        if family == Family::Inet6 {
            let _ = sock.set_only_v6(false);
        }

        Ok(TsiDgramProxy {
            id,
            cid,
            local_port: 0,
            peer_port,
            sock,
            status: ProxyStatus::Idle,
            sendto_addr: None,
            listening: false,
            family,
            mem,
            queue,
            rxq,
            snapshot_gate,
            rx_cnt: Wrapping(0),
            tx_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            dns_sender,
            connected_dns: false,
        })
    }

    /// Hand a guest DNS query to the worker thread instead of sending it out the
    /// host socket. `tx_cnt` is advanced by the query length so the response's
    /// `fwd_cnt` stays monotonic — identical to the normal send paths — without
    /// the worker ever touching per-proxy state.
    fn send_dns_query(&mut self, buf: &[u8]) {
        self.tx_cnt += Wrapping(buf.len() as u32);
        if let Some(sender) = &self.dns_sender {
            let _ = sender.send(DnsRequest {
                query: buf.to_vec(),
                peer_port: self.peer_port,
                fwd_cnt: self.tx_cnt.0,
                cid: self.cid,
            });
        }
    }

    fn init_pkt(&self, pkt: &mut VsockPacket) {
        debug!(
            "init_pkt: id={}, src_port={}, dst_port={}",
            self.id, self.local_port, self.peer_port
        );
        pkt.set_op(uapi::VSOCK_OP_RW)
            .set_src_cid(self.cid)
            .set_dst_cid(uapi::VSOCK_HOST_CID)
            .set_dst_port(self.peer_port)
            .set_src_port(0)
            .set_type(uapi::VSOCK_TYPE_DGRAM)
            .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32)
            .set_fwd_cnt(self.tx_cnt.0);
    }

    fn recv_to_pkt(&self, pkt: &mut VsockPacket) -> RecvPkt {
        if let Some(buf) = pkt.buf_mut() {
            let max_len = buf.len();
            match sys::recv_into(&self.sock, &mut buf[..max_len]) {
                Ok(cnt) => {
                    debug!("recv cnt={cnt}");
                    if cnt > 0 {
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
        let _snapshot_activity = self.snapshot_gate.enter();
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
                        self.init_pkt(&mut pkt);
                        pkt.set_len(cnt as u32);
                        pkt.hdr().len() + cnt
                    }
                    RecvPkt::Close => {
                        self.status = ProxyStatus::Closed;
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
                debug!("recv_pkt: pushing packet with {len} bytes");
                if let Err(e) = queue.add_used(&self.mem, head.index, len as u32) {
                    error!("failed to add used elements to the queue: {e:?}");
                }
            }
        }

        debug!("recv_pkt: have_used={have_used}");
        (have_used, wait_credit)
    }

    /// Bind the datagram socket to an unspecified local address so it can
    /// receive replies. Returns the bind result.
    fn bind_unspecified(&self) -> std::io::Result<()> {
        match self.family {
            Family::Inet => self
                .sock
                .bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()),
            Family::Inet6 => self.sock.bind(
                &std::net::SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, 0, 0, 0).into(),
            ),
            #[cfg(unix)]
            Family::Unix => {
                // Autobind an unnamed AF_UNIX datagram socket (zero-length path).
                let mut sun: libc::sockaddr_un = unsafe { std::mem::zeroed() };
                sun.sun_family = libc::AF_UNIX as _;
                let len = std::mem::size_of::<libc::sa_family_t>() as libc::socklen_t;
                let rc = unsafe {
                    libc::bind(
                        raw_handle(&self.sock),
                        &sun as *const _ as *const libc::sockaddr,
                        len,
                    )
                };
                if rc == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            }
        }
    }
}

impl Proxy for TsiDgramProxy {
    fn poll_handle(&self) -> ProxyRawHandle {
        raw_handle(&self.sock)
    }

    fn id(&self) -> u64 {
        self.id
    }

    fn status(&self) -> ProxyStatus {
        self.status
    }

    fn is_dgram(&self) -> bool {
        true
    }

    fn connect(&mut self, pkt: &VsockPacket, req: TsiConnectReq) -> ProxyUpdate {
        debug!("connect: addr={}", req.addr);

        // A connected DNS socket is intercepted: its sends are routed to the
        // DNS worker, which queries only the host forwarder or the
        // host-trusted resolvers, never the guest-chosen one. The host socket
        // is still connected (a datagram connect sends nothing, it only looks
        // up the route), so a resolver the host cannot reach fails the connect
        // exactly as it would without interception, and the guest's resolver
        // and address selection see the same errors.
        let intercept = self.dns_sender.is_some() && sockaddr_port(&req.addr) == Some(DNS_PORT);
        let addr: std::io::Result<socket2::SockAddr> = match req.addr.inet() {
            _ if !req.addr.suits_family(self.family) => {
                warn!(
                    "connect: id={} refused address {} for its family",
                    self.id, req.addr
                );
                Err(std::io::Error::from_raw_os_error(libc::EINVAL))
            }
            Some(inet) => Ok(inet.into()),
            #[cfg(target_os = "linux")]
            None => match req.addr.unix_path() {
                Some(p) => socket2::SockAddr::unix(p),
                None => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
            },
            #[cfg(not(target_os = "linux"))]
            None => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
        };
        let connected =
            if self.family == Family::Inet && req.addr.inet().is_some_and(|a| a.is_ipv6()) {
                // Linux rejects this before changing the socket. Darwin can
                // disconnect an existing UDP peer before returning the error.
                // This is already a guest errno, not a native Winsock/BSD error.
                Err(LINUX_EAFNOSUPPORT)
            } else {
                addr.and_then(|a| self.sock.connect(&a))
                    .map_err(|err| sys::to_linux_errno(&err))
            };
        let (res, poll_host_socket) = match connected {
            Ok(()) => {
                // Keep the previous DNS policy until the new connection succeeds.
                self.connected_dns = intercept;
                self.status = ProxyStatus::Connected;
                if intercept {
                    debug!("connect: intercepting DNS socket");
                    (0, false)
                } else {
                    debug!("connect: Connected");
                    (0, true)
                }
            }
            Err(errno) => {
                debug!("Error connecting: Linux errno {errno}");
                (-errno, false)
            }
        };

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        // This response goes to the connection.
        let rx = MuxerRx::ConnResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
            result: res,
        };
        push_packet(
            self.cid,
            rx,
            &self.rxq,
            &self.queue,
            &self.mem,
            &self.snapshot_gate,
        );

        let mut update = ProxyUpdate::default();
        if poll_host_socket && !self.listening {
            update.polling = Some((self.id, raw_handle(&self.sock), EventSet::IN));
        }
        update
    }

    fn getpeername(&mut self, pkt: &VsockPacket) {
        debug!("process_getpeername");

        let (result, addr): (i32, VsockAddr) = match self.sock.peer_addr() {
            Ok(sa) => match sa.as_socket() {
                Some(socket_addr) => (0, VsockAddr::Inet(socket_addr)),
                None => (
                    -libc::EINVAL,
                    VsockAddr::Inet(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()),
                ),
            },
            Err(e) => (
                -sys::to_linux_errno(&e),
                VsockAddr::Inet(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()),
            ),
        };

        let data = TsiGetnameRsp {
            result,
            addr_len: addr.linux_len(),
            addr,
        };

        // This response goes to the connection.
        let rx = MuxerRx::GetnameResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
            data,
        };
        push_packet(
            self.cid,
            rx,
            &self.rxq,
            &self.queue,
            &self.mem,
            &self.snapshot_gate,
        );
    }

    fn sendmsg(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        debug!("sendmsg");

        // Connected DNS socket: route the query to the worker; the worker pushes
        // the response and signals the guest asynchronously.
        if self.connected_dns {
            if let Some(buf) = pkt.data() {
                self.send_dns_query(buf);
            }
            return ProxyUpdate::default();
        }

        let ret = if let Some(buf) = pkt.data() {
            #[cfg(target_os = "linux")]
            let send_res = self.sock.send_with_flags(buf, libc::MSG_NOSIGNAL);
            #[cfg(not(target_os = "linux"))]
            let send_res = self.sock.send(buf);

            match send_res {
                Ok(sent) => {
                    self.tx_cnt += Wrapping(sent as u32);
                    sent as i32
                }
                Err(err) => -sys::to_linux_errno(&err),
            }
        } else {
            -libc::EINVAL
        };

        debug!("sendmsg ret={ret}");

        ProxyUpdate::default()
    }

    fn sendto_addr(&mut self, req: TsiSendtoAddr) -> ProxyUpdate {
        debug!("sendto_addr: addr={}", req.addr);

        let mut update = ProxyUpdate::default();

        if !req.addr.suits_family(self.family) {
            warn!(
                "sendto: id={} refused address {} for its family",
                self.id, req.addr
            );
            self.sendto_addr = None;
            return update;
        }
        self.sendto_addr = Some(req.addr);
        if !self.listening {
            match self.bind_unspecified() {
                Ok(_) => {
                    self.listening = true;
                    update.polling = Some((self.id, raw_handle(&self.sock), EventSet::IN));
                }
                Err(e) => debug!("couldn't bind socket: {e}"),
            }
        }

        update
    }

    fn sendto_data(&mut self, pkt: &VsockPacket) {
        debug!("sendto_data");

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        if let Some(addr) = self.sendto_addr.clone() {
            if let Some(buf) = pkt.data() {
                // Unconnected DNS query (sendto a resolver:53): route to the
                // worker instead of the host socket.
                if self.dns_sender.is_some() && sockaddr_port(&addr) == Some(DNS_PORT) {
                    self.send_dns_query(buf);
                    return;
                }

                let dst: Option<socket2::SockAddr> = match addr.inet() {
                    Some(inet) => Some(inet.into()),
                    #[cfg(target_os = "linux")]
                    None => addr
                        .unix_path()
                        .and_then(|p| socket2::SockAddr::unix(p).ok()),
                    #[cfg(not(target_os = "linux"))]
                    None => None,
                };

                match dst {
                    Some(d) => match self.sock.send_to(buf, &d) {
                        Ok(sent) => {
                            self.tx_cnt += Wrapping(sent as u32);
                        }
                        Err(err) => debug!("error in sendto: {err}"),
                    },
                    None => debug!("sendto_data: unsupported destination address"),
                }
            } else {
                debug!("sendto_data pkt without buffer");
            }
        } else {
            debug!("sendto_data without sendto_addr");
        }
    }

    fn listen(
        &mut self,
        _pkt: &VsockPacket,
        _req: TsiListenReq,
        _host_port_map: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn accept(&mut self, _req: TsiAcceptReq) -> ProxyUpdate {
        ProxyUpdate::default()
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

        ProxyUpdate {
            polling: Some((self.id, raw_handle(&self.sock), EventSet::IN)),
            ..Default::default()
        }
    }

    fn process_op_response(&mut self, _pkt: &VsockPacket) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn release(&mut self) -> ProxyUpdate {
        debug!("release");
        // The guest closed its datagram socket; nothing of it is left to
        // deliver, so its host socket goes now rather than after a grace period.
        let remove_proxy = ProxyRemoval::Immediate;
        ProxyUpdate {
            remove_proxy,
            ..Default::default()
        }
    }

    fn process_event(&mut self, evset: EventSet) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        if evset.contains(EventSet::HANG_UP) {
            update.remove_proxy = if self.status == ProxyStatus::Listening {
                ProxyRemoval::Immediate
            } else {
                ProxyRemoval::Deferred
            };
            return update;
        }

        if evset.contains(EventSet::IN) {
            let (signal_queue, wait_credit) = self.recv_pkt();
            update.signal_queue = signal_queue || wait_credit;

            if wait_credit && self.status != ProxyStatus::WaitingCreditUpdate {
                self.status = ProxyStatus::WaitingCreditUpdate;
                let rx = MuxerRx::CreditRequest {
                    local_port: self.local_port,
                    peer_port: self.peer_port,
                    fwd_cnt: self.tx_cnt.0,
                };
                update.push_credit_req = Some(rx);
            }

            if self.status == ProxyStatus::WaitingCreditUpdate {
                debug!("process_event: WaitingCreditUpdate");
                update.polling = Some((self.id(), raw_handle(&self.sock), EventSet::empty()));
            }
        }

        if evset.contains(EventSet::OUT) {
            error!("EventSet::OUT unexpected");
        }

        update
    }
}

#[cfg(test)]
mod tests {
    use super::super::packet::VSOCK_PKT_HDR_SIZE;
    use super::*;
    use crate::virtio::queue::tests::VirtQueue as TestQueue;
    use std::net::{SocketAddr, UdpSocket};
    use std::time::Duration;
    use vm_memory::{Bytes, GuestAddress};

    #[test]
    fn failed_reconnect_preserves_dns_routing_and_successful_reconnect_replaces_it() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x40000)]).unwrap();
        let rx = TestQueue::new(GuestAddress(0x4000), &mem, 8);
        let replies = Arc::new(Mutex::new(MuxerRxQ::new()));
        let (sender, requests) = crossbeam_channel::unbounded();
        let mut proxy = TsiDgramProxy::new(
            1,
            3,
            defs::LINUX_AF_INET,
            7000,
            mem.clone(),
            Arc::new(Mutex::new(rx.create_queue())),
            replies.clone(),
            Some(sender),
            Arc::new(SnapshotGate::default()),
        )
        .unwrap();
        let packet = |data: &[u8]| {
            let tx = TestQueue::new(GuestAddress(0x1000), &mem, 8);
            mem.write_slice(&[0; VSOCK_PKT_HDR_SIZE], GuestAddress(0x10000))
                .unwrap();
            mem.write_obj(data.len() as u32, GuestAddress(0x10000 + 24))
                .unwrap();
            mem.write_slice(data, GuestAddress(0x10000 + VSOCK_PKT_HDR_SIZE as u64))
                .unwrap();
            tx.dtable[0].set(0x10000, (VSOCK_PKT_HDR_SIZE + data.len()) as u32, 0, 0);
            tx.avail.ring[0].set(0);
            tx.avail.idx.set(1);
            let mut pkt =
                VsockPacket::from_tx_virtq_head(&tx.create_queue().pop(&mem).unwrap()).unwrap();
            pkt.set_op(uapi::VSOCK_OP_RW)
                .set_src_port(7000)
                .set_dst_port(defs::TSI_PROXY_PORT)
                .set_type(uapi::VSOCK_TYPE_DGRAM);
            pkt
        };
        let resolver: SocketAddr = "127.0.0.1:53".parse().unwrap();
        for (destination, expected) in [(resolver, 0), ("[::1]:53".parse().unwrap(), -97)] {
            proxy.connect(
                &packet(&[]),
                TsiConnectReq {
                    peer_port: 7000,
                    addr: destination.into(),
                },
            );
            let reply = replies.lock().unwrap().pop();
            match reply {
                Some(MuxerRx::ConnResponse { result, .. }) => assert_eq!(result, expected),
                other => panic!("unexpected connect response: {other:?}"),
            }
        }
        assert_eq!(proxy.sock.peer_addr().unwrap().as_socket(), Some(resolver));
        let bound = proxy.sock.local_addr().unwrap().as_socket().unwrap();
        proxy.sendmsg(&packet(b"query"));
        let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(request.query, b"query");
        assert_eq!(request.peer_port, 7000);

        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        proxy.connect(
            &packet(&[]),
            TsiConnectReq {
                peer_port: 7000,
                addr: peer.local_addr().unwrap().into(),
            },
        );
        assert!(matches!(
            replies.lock().unwrap().pop(),
            Some(MuxerRx::ConnResponse { result: 0, .. })
        ));
        // The same invalid reconnect must also preserve an ordinary UDP peer.
        proxy.connect(
            &packet(&[]),
            TsiConnectReq {
                peer_port: 7000,
                addr: "[::1]:53".parse::<SocketAddr>().unwrap().into(),
            },
        );
        assert!(matches!(
            replies.lock().unwrap().pop(),
            Some(MuxerRx::ConnResponse { result: -97, .. })
        ));
        assert_eq!(
            proxy.sock.peer_addr().unwrap().as_socket(),
            Some(peer.local_addr().unwrap())
        );
        proxy.sendmsg(&packet(b"data"));
        let mut bytes = [0; 16];
        let (length, source) = peer.recv_from(&mut bytes).unwrap();
        assert_eq!(&bytes[..length], b"data");
        assert_eq!(source, bound);
        assert!(requests.try_recv().is_err());
    }
}
