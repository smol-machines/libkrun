use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

const REQUEST: &[u8] = b"ping!";
const RESPONSE: &[u8] = b"pong!";
const FOLLOWUP: &[u8] = b"bye!";

fn set_timeouts(sock: &UdpSocket) {
    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    sock.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
}

#[derive(Debug, Copy, Clone)]
pub struct UdpTester {
    port: u16,
}

#[allow(dead_code)] // host build only uses the server side, guest only the client side
impl UdpTester {
    pub const fn new(port: u16) -> Self {
        Self { port }
    }

    /// Bind a server socket on the host's loopback interface. Run on the host
    /// side; the returned socket is then driven by [`run_server`].
    pub fn create_server_socket(&self) -> UdpSocket {
        let addr = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), self.port);
        UdpSocket::bind(addr).unwrap()
    }

    /// Drive the server side of the protocol: wait for the client's REQUEST,
    /// reply with RESPONSE, then send a follow-up datagram (FOLLOWUP). The
    /// follow-up exercises the host -> guest path independently from the
    /// echo path so we don't depend on a single round trip.
    pub fn run_server(&self, sock: UdpSocket) {
        set_timeouts(&sock);
        let mut buf = [0u8; 64];
        let (n, peer) = sock.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], REQUEST, "server: unexpected request");
        sock.send_to(RESPONSE, peer).unwrap();
        sock.send_to(FOLLOWUP, peer).unwrap();
    }

    /// Drive the guest side: bind a fresh socket, sendto the server, receive
    /// the RESPONSE, then receive the FOLLOWUP. recv_from must report the
    /// real server address (this is what TSI's address tracking is for).
    pub fn run_client(&self) {
        let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)).unwrap();
        set_timeouts(&sock);
        let server: SocketAddr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.port);

        sock.send_to(REQUEST, server).unwrap();

        let mut buf = [0u8; 64];
        let (n, addr) = sock.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], RESPONSE, "client: bad RESPONSE payload");
        assert_eq!(addr, server, "client: RESPONSE source address mismatch");

        let (n, addr) = sock.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], FOLLOWUP, "client: bad FOLLOWUP payload");
        assert_eq!(addr, server, "client: FOLLOWUP source address mismatch");
    }
}
