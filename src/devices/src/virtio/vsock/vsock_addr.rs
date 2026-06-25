// Copyright 2026 The libkrun Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Cross-platform socket address for the TSI guest ABI.
//!
//! The guest always speaks the **Linux** sockaddr wire format (AF_INET = 2,
//! AF_INET6 = 10; port and address in network byte order). The host, however,
//! may lay sockaddrs out differently (macOS uses a `sa_len` byte and different
//! AF_* values; Windows uses AF_INET6 = 23). The existing code carried a
//! `nix::SockaddrStorage` and patched the bytes per-OS, but `nix` does not build
//! on Windows at all.
//!
//! [`VsockAddr`] replaces `SockaddrStorage` in the vsock/TSI path: it parses the
//! guest's Linux-layout bytes into a host-neutral [`std::net::SocketAddr`] and
//! serializes back to Linux layout regardless of host, so the guest ABI stays
//! identical on every platform and no per-OS patching is needed. The inner
//! `SocketAddr` feeds `socket2` for the actual host socket calls.
//!
//! See WINDOWS_NETWORKING_PORT.md (Phase 1).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use super::defs;

/// Length of a Linux `struct sockaddr_in`.
const LINUX_SOCKADDR_IN_LEN: usize = 16;
/// Length of a Linux `struct sockaddr_in6`.
const LINUX_SOCKADDR_IN6_LEN: usize = 28;

/// A socket address in the guest's (Linux) sockaddr ABI, stored host-neutrally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VsockAddr(SocketAddr);

#[allow(dead_code)]
impl VsockAddr {
    /// Parse the first `addr_len` bytes of a guest-supplied Linux sockaddr.
    ///
    /// `buf` and `addr_len` are guest-controlled, so every read is bounds-checked
    /// against both `addr_len` and the actual buffer length. Returns `None` for a
    /// truncated buffer or an unsupported address family (AF_UNIX is handled
    /// separately on the platforms that support it).
    pub fn parse_linux(buf: &[u8], addr_len: u32) -> Option<Self> {
        let len = addr_len as usize;
        if len < 2 || len > buf.len() {
            return None;
        }
        let family = u16::from_le_bytes([buf[0], buf[1]]);
        match family {
            defs::LINUX_AF_INET => {
                if buf.len() < LINUX_SOCKADDR_IN_LEN {
                    return None;
                }
                let port = u16::from_be_bytes([buf[2], buf[3]]);
                let ip = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
                Some(VsockAddr(SocketAddrV4::new(ip, port).into()))
            }
            defs::LINUX_AF_INET6 => {
                if buf.len() < LINUX_SOCKADDR_IN6_LEN {
                    return None;
                }
                let port = u16::from_be_bytes([buf[2], buf[3]]);
                let flowinfo = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&buf[8..24]);
                let scope_id = u32::from_be_bytes([buf[24], buf[25], buf[26], buf[27]]);
                Some(VsockAddr(
                    SocketAddrV6::new(Ipv6Addr::from(octets), port, flowinfo, scope_id).into(),
                ))
            }
            _ => None,
        }
    }

    /// Serialize to the guest's Linux sockaddr wire format (`sockaddr_in` for
    /// IPv4, `sockaddr_in6` for IPv6). Always emits Linux layout regardless of
    /// the host, so the value can be copied straight into a guest getname reply.
    pub fn to_linux_bytes(self) -> Vec<u8> {
        match self.0 {
            SocketAddr::V4(a) => {
                let mut b = vec![0u8; LINUX_SOCKADDR_IN_LEN];
                b[0..2].copy_from_slice(&defs::LINUX_AF_INET.to_le_bytes());
                b[2..4].copy_from_slice(&a.port().to_be_bytes());
                b[4..8].copy_from_slice(&a.ip().octets());
                b
            }
            SocketAddr::V6(a) => {
                let mut b = vec![0u8; LINUX_SOCKADDR_IN6_LEN];
                b[0..2].copy_from_slice(&defs::LINUX_AF_INET6.to_le_bytes());
                b[2..4].copy_from_slice(&a.port().to_be_bytes());
                b[4..8].copy_from_slice(&a.flowinfo().to_be_bytes());
                b[8..24].copy_from_slice(&a.ip().octets());
                b[24..28].copy_from_slice(&a.scope_id().to_be_bytes());
                b
            }
        }
    }

    /// Length of the Linux sockaddr this address serializes to.
    pub fn linux_len(&self) -> u32 {
        match self.0 {
            SocketAddr::V4(_) => LINUX_SOCKADDR_IN_LEN as u32,
            SocketAddr::V6(_) => LINUX_SOCKADDR_IN6_LEN as u32,
        }
    }

    /// The host-neutral address, for handing to `socket2`/`std::net`.
    pub fn socket_addr(&self) -> SocketAddr {
        self.0
    }
}

impl From<SocketAddr> for VsockAddr {
    fn from(addr: SocketAddr) -> Self {
        VsockAddr(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_round_trip() {
        let buf = [
            2, 0, // AF_INET (LE)
            0x1f, 0x90, // port 8080 (BE)
            127, 0, 0, 1, // 127.0.0.1
            0, 0, 0, 0, 0, 0, 0, 0, // padding
        ];
        let addr = VsockAddr::parse_linux(&buf, 16).unwrap();
        assert_eq!(
            addr.socket_addr(),
            "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(addr.linux_len(), 16);
        // Serialized bytes match the meaningful prefix of the input.
        assert_eq!(&addr.to_linux_bytes()[..8], &buf[..8]);
    }

    #[test]
    fn ipv6_round_trip() {
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let original = VsockAddr(SocketAddrV6::new(ip, 443, 0, 7).into());
        let bytes = original.to_linux_bytes();
        assert_eq!(bytes.len(), 28);
        assert_eq!(
            u16::from_le_bytes([bytes[0], bytes[1]]),
            defs::LINUX_AF_INET6
        );
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 443);

        let parsed = VsockAddr::parse_linux(&bytes, 28).unwrap();
        assert_eq!(parsed, original);
        match parsed.socket_addr() {
            SocketAddr::V6(a) => {
                assert_eq!(*a.ip(), ip);
                assert_eq!(a.port(), 443);
                assert_eq!(a.scope_id(), 7);
            }
            _ => panic!("expected v6"),
        }
    }

    #[test]
    fn rejects_short_and_unknown() {
        // addr_len past the buffer.
        assert!(VsockAddr::parse_linux(&[2, 0, 0, 0], 16).is_none());
        // Too few bytes for the claimed family.
        assert!(VsockAddr::parse_linux(&[2, 0, 0], 3).is_none());
        // Unknown family.
        let mut buf = [0u8; 16];
        buf[0] = 99;
        assert!(VsockAddr::parse_linux(&buf, 16).is_none());
        // Zero length.
        assert!(VsockAddr::parse_linux(&buf, 0).is_none());
    }

    #[test]
    fn ipv4_port_and_addr_byte_order() {
        let addr = VsockAddr(SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), 53).into());
        let b = addr.to_linux_bytes();
        // Family little-endian, port big-endian, address in order.
        assert_eq!(&b[0..2], &[2, 0]);
        assert_eq!(&b[2..4], &53u16.to_be_bytes());
        assert_eq!(&b[4..8], &[10, 0, 2, 2]);
    }
}
