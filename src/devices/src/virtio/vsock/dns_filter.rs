use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, RwLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::socket::SockaddrStorage;

const DNS_PORT: u16 = 53;
const DNS_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_DNS_PACKET_SIZE: usize = 65_535;
const MIN_LEARNED_TTL: u64 = 30;
const MAX_LEARNED_TTL: u64 = 300;
const DEFAULT_EGRESS_REFRESH_PER_SECS: u32 = 5 * 60;
const DYNAMIC_ALLOWED_IP_CAP: usize = 512;
const MAX_OBSERVED_DNS_RESOLVERS: usize = 3;
const DNS_HEADER_LEN: usize = 12;
const DNS_ID_LEN: usize = 2;
const DNS_U16_LEN: usize = 2;
const DNS_U32_LEN: usize = 4;
const DNS_FLAGS_OFFSET: usize = 2;
const DNS_QDCOUNT_OFFSET: usize = 4;
const DNS_ANCOUNT_OFFSET: usize = 6;
const DNS_QUESTION_FIXED_LEN: usize = 4;
const DNS_RR_FIXED_LEN: usize = 10;
const DNS_RR_TYPE_OFFSET: usize = 0;
const DNS_RR_CLASS_OFFSET: usize = 2;
const DNS_RR_TTL_OFFSET: usize = 4;
const DNS_RR_RDLEN_OFFSET: usize = 8;
const DNS_CLASS_IN: u16 = 1;
const DNS_TYPE_A: u16 = 1;
const DNS_TYPE_AAAA: u16 = 28;
const DNS_A_RDATA_LEN: usize = 4;
const DNS_AAAA_RDATA_LEN: usize = 16;
const DNS_RCODE_SERVFAIL: u16 = 2;
const DNS_RCODE_NXDOMAIN: u16 = 3;
const DNS_RCODE_MASK: u16 = 0x000f;
const DNS_COUNT_ZERO: u16 = 0;
const DNS_ONE_QUESTION: u16 = 1;
const DNS_FLAG_RESPONSE: u16 = 0x8000;
const DNS_FLAG_RECURSION_DESIRED: u16 = 0x0100;
const DNS_FLAG_RECURSION_AVAILABLE: u16 = 0x0080;
const DNS_POINTER_TAG: u8 = 0xc0;
const DNS_POINTER_MASK: u8 = 0xc0;
const DNS_POINTER_OFFSET_MASK: u8 = 0x3f;
const DNS_MAX_COMPRESSION_JUMPS: usize = 16;
const DNS_MAX_LABEL_LEN: usize = 63;

#[derive(Debug)]
pub(super) struct EgressPolicy {
    allowed_cidrs: Vec<(IpAddr, u8)>,
    allowed_hosts: Option<Vec<String>>,
    // allowed_ips are learned from the on-demand dns queries of the allowed_hosts. The key is 
    // the IpAddr and the value is the expiration of this resolved ip. We update this map by
    // removing the ips that have expired and upserting the ips with the latest expiration.
    allowed_ips: HashMap<IpAddr, Instant>,
    // The DNS servers that are seen by the guest. DNS servers can be configured by both
    // the guest resolve.conf or the resolve.conf in the container. Whenever the app in 
    // guest initiate a DNS query, it invokes either a UDP connect (used by nslookup) or
    // send_to (libc UDP socket send_to). This is when TSI gets to know what DNS servers are
    // used by the guest. This guest DNS server ip is saved in this dns_resolver, and then
    // for a DNS query, we check whether the parsed hostname is in the list of allowed hosts
    // This approach ensures the egress policies are enforced in guest based on the actual
    // DNS servers used by the guest.
    dns_resolvers: Vec<SocketAddr>,
}

#[derive(Debug)]
struct DnsQuestion {
    name: String,
    end: usize,
}

impl EgressPolicy {
    pub(super) fn new(
        allowed_cidrs: Option<Vec<(IpAddr, u8)>>,
        egress_hosts: Option<Vec<String>>,
    ) -> Option<Self> {
        if allowed_cidrs.is_none() && egress_hosts.is_none() {
            return None;
        }

        let egress_hosts = egress_hosts.map(|hosts| {
            hosts
                .into_iter()
                .filter_map(|host| normalize_hostname(&host))
                .collect()
        });

        Some(Self {
            allowed_cidrs: allowed_cidrs.unwrap_or_default(),
            allowed_ips: HashMap::new(),
            allowed_hosts: egress_hosts,
            dns_resolvers: Vec::new(),
        })
    }

    pub(super) fn dns_filter_enabled(&self) -> bool {
        self.allowed_hosts.is_some()
    }

    // Identify whether a UDP request is for DNS query by check if the target port is 53
    pub(super) fn should_intercept_dns(&self, addr: &SockaddrStorage) -> bool {
        self.dns_filter_enabled() && sockaddr_port(addr) == Some(DNS_PORT)
    }

    pub(super) fn is_addr_allowed(&self, addr: &SockaddrStorage) -> bool {
        let Some(ip) = sockaddr_ip(addr) else {
            return true;
        };
        
        // allow this ip if it is within the provided cidrs which are static
        if ip_matches_cidrs(ip, &self.allowed_cidrs) {
            return true;
        }

        // allow this ip if it is the from the dns query of any allow_hosts and it hasn't expire.
        self.allowed_ips
            .get(&ip)
            .is_some_and(|expires_at| *expires_at > Instant::now())
    }

    fn is_hostname_allowed(&self, hostname: &str) -> bool {
        let Some(allowed_hosts) = &self.allowed_hosts else {
            return true;
        };

        let Some(hostname) = normalize_hostname(hostname) else {
            return false;
        };

        allowed_hosts.iter().any(|allowed| {
            hostname == *allowed
                || hostname
                    .strip_suffix(allowed)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    }

    fn learn_ips(&mut self, ips: Vec<(IpAddr, u32)>) {
        let now = Instant::now();
        self.prune_allowed_ips(now);

        for (ip, ttl) in ips {
            let ttl = u64::from(ttl).clamp(MIN_LEARNED_TTL, MAX_LEARNED_TTL);
            let expires_at = now + Duration::from_secs(ttl);
            self.allowed_ips
                .entry(ip)
                .and_modify(|existing| *existing = (*existing).max(expires_at))
                .or_insert(expires_at);
        }
    }

    fn prune_allowed_ips(&mut self, now: Instant) {
        self.allowed_ips
            .retain(|_, expires_at| *expires_at > now);
    }

    fn remember_dns_resolver(&mut self, resolver: SocketAddr) {
        self.dns_resolvers.retain(|existing| *existing != resolver);
        self.dns_resolvers.insert(0, resolver);
        self.dns_resolvers.truncate(MAX_OBSERVED_DNS_RESOLVERS);
    }
}

pub(super) fn start_host_refresh(
    policy: &Arc<RwLock<EgressPolicy>>,
    egress_refresh_per_secs: Option<u32>,
) {
    let hosts = match policy.read() {
        Ok(policy) => policy.allowed_hosts.clone().unwrap_or_default(),
        Err(_) => return,
    };

    if hosts.is_empty() {
        return;
    }

    let interval = Duration::from_secs(u64::from(
        egress_refresh_per_secs
            .unwrap_or(DEFAULT_EGRESS_REFRESH_PER_SECS)
            .max(1),
    ));
    let weak_policy = Arc::downgrade(policy);

    if let Err(err) = thread::Builder::new()
        .name("tsi-egress-refresh".into())
        .spawn(move || refresh_loop(weak_policy, hosts, interval))
    {
        debug!("failed to spawn egress refresh thread: {err}");
    }
}

fn refresh_loop(policy: Weak<RwLock<EgressPolicy>>, hosts: Vec<String>, interval: Duration) {
    while let Some(policy) = policy.upgrade() {
        refresh_once(&policy, &hosts);
        thread::sleep(interval);
    }
}

fn refresh_once(policy: &Arc<RwLock<EgressPolicy>>, hosts: &[String]) {
    let resolvers = match policy.read() {
        Ok(policy) => policy.dns_resolvers.clone(),
        Err(_) => return,
    };
    if resolvers.is_empty() {
        return;
    }

    let ips = resolve_hosts_to_ips(hosts, &resolvers);
    if ips.is_empty() {
        return;
    }

    if let Ok(mut policy) = policy.write() {
        policy.learn_ips(ips);
    }
}

fn resolve_hosts_to_ips(hosts: &[String], resolvers: &[SocketAddr]) -> Vec<(IpAddr, u32)> {
    let mut ips = Vec::new();

    'hosts: for host in hosts {
        if let Ok(ip) = host.parse::<IpAddr>() {
            push_ip(&mut ips, ip, MAX_LEARNED_TTL as u32);
            if ips.len() >= DYNAMIC_ALLOWED_IP_CAP {
                break;
            }
            continue;
        }

        for resolver in resolvers {
            for qtype in [DNS_TYPE_A, DNS_TYPE_AAAA] {
                let query = match build_query(host, qtype) {
                    Some(query) => query,
                    None => continue 'hosts,
                };
                let response = match forward_dns_udp(&query, *resolver) {
                    Ok(response) => response,
                    Err(err) => {
                        debug!("egress refresh failed to resolve {host} via {resolver}: {err}");
                        continue;
                    }
                };
                if let Ok(answer_ips) = parse_answer_ips(&response) {
                    for (ip, ttl) in answer_ips {
                        push_ip(&mut ips, ip, ttl);
                        if ips.len() >= DYNAMIC_ALLOWED_IP_CAP {
                            break 'hosts;
                        }
                    }
                }
            }
        }
    }

    ips
}

fn build_query(hostname: &str, qtype: u16) -> Option<Vec<u8>> {
    let hostname = normalize_hostname(hostname)?;
    let mut query = vec![
        0x12,
        0x34, // ID
        (DNS_FLAG_RECURSION_DESIRED >> 8) as u8,
        DNS_FLAG_RECURSION_DESIRED as u8,
        0x00,
        0x01, // QDCOUNT
        0x00,
        0x00, // ANCOUNT
        0x00,
        0x00, // NSCOUNT
        0x00,
        0x00, // ARCOUNT
    ];

    for label in hostname.split('.') {
        if label.is_empty() || label.len() > DNS_MAX_LABEL_LEN {
            return None;
        }
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&qtype.to_be_bytes());
    query.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());

    Some(query)
}

fn push_ip(ips: &mut Vec<(IpAddr, u32)>, ip: IpAddr, ttl: u32) {
    if let Some((_, existing_ttl)) = ips.iter_mut().find(|(existing_ip, _)| *existing_ip == ip) {
        *existing_ttl = (*existing_ttl).max(ttl);
    } else {
        ips.push((ip, ttl));
    }
}

pub(super) fn handle_dns_query(
    policy: &Arc<RwLock<EgressPolicy>>,
    query: &[u8],
    resolver: &SockaddrStorage,
) -> Vec<u8> {
    let question = match parse_question(query) {
        Ok(question) => question,
        Err(_) => return build_error_response(query, DNS_RCODE_SERVFAIL),
    };

    let allowed = policy
        .read()
        .map(|policy| policy.is_hostname_allowed(&question.name))
        .unwrap_or(false);

    if !allowed {
        return build_error_response(query, DNS_RCODE_NXDOMAIN);
    }

    let Some(resolver) = sockaddr_to_socket_addr(resolver) else {
        return build_error_response(query, DNS_RCODE_SERVFAIL);
    };

    if let Ok(mut policy) = policy.write() {
        policy.remember_dns_resolver(resolver);
    }

    match forward_dns_udp(query, resolver) {
        Ok(response) => {
            if let Ok(ips) = parse_answer_ips(&response) {
                if !ips.is_empty() {
                    if let Ok(mut policy) = policy.write() {
                        policy.learn_ips(ips);
                    }
                }
            }
            response
        }
        Err(err) => {
            debug!("DNS upstream query failed: {err}");
            build_error_response(query, DNS_RCODE_SERVFAIL)
        }
    }
}

pub(super) fn sockaddr_port(addr: &SockaddrStorage) -> Option<u16> {
    match (addr.as_sockaddr_in(), addr.as_sockaddr_in6()) {
        (Some(sin), _) => Some(sin.port()),
        (_, Some(sin6)) => Some(sin6.port()),
        _ => None,
    }
}

fn sockaddr_ip(addr: &SockaddrStorage) -> Option<IpAddr> {
    match (addr.as_sockaddr_in(), addr.as_sockaddr_in6()) {
        (Some(sin), _) => Some(IpAddr::V4(sin.ip())),
        (_, Some(sin6)) => Some(IpAddr::V6(sin6.ip())),
        _ => None,
    }
}

fn sockaddr_to_socket_addr(addr: &SockaddrStorage) -> Option<SocketAddr> {
    match (addr.as_sockaddr_in(), addr.as_sockaddr_in6()) {
        (Some(sin), _) => Some(SocketAddr::new(IpAddr::V4(sin.ip()), sin.port())),
        (_, Some(sin6)) => Some(SocketAddr::new(IpAddr::V6(sin6.ip()), sin6.port())),
        _ => None,
    }
}

pub(super) fn ip_matches_cidrs(ip: IpAddr, cidrs: &[(IpAddr, u8)]) -> bool {
    for (cidr_ip, prefix_len) in cidrs {
        match (ip, cidr_ip) {
            (IpAddr::V4(addr_v4), IpAddr::V4(cidr_v4)) => {
                let mask = match *prefix_len {
                    0 => 0u32,
                    p if p >= 32 => u32::MAX,
                    _ => u32::MAX << (32 - prefix_len),
                };
                let addr_bits = u32::from(addr_v4);
                let cidr_bits = u32::from(*cidr_v4);
                if addr_bits & mask == cidr_bits & mask {
                    return true;
                }
            }
            (IpAddr::V6(addr_v6), IpAddr::V6(cidr_v6)) => {
                let mask = match *prefix_len {
                    0 => 0u128,
                    p if p >= 128 => u128::MAX,
                    _ => u128::MAX << (128 - prefix_len),
                };
                let addr_bits = u128::from(addr_v6);
                let cidr_bits = u128::from(*cidr_v6);
                if addr_bits & mask == cidr_bits & mask {
                    return true;
                }
            }
            _ => {}
        }
    }

    false
}

fn normalize_hostname(hostname: &str) -> Option<String> {
    let hostname = hostname.trim_end_matches('.').to_ascii_lowercase();
    if hostname.is_empty() {
        None
    } else {
        Some(hostname)
    }
}

fn forward_dns_udp(query: &[u8], resolver: SocketAddr) -> std::io::Result<Vec<u8>> {
    let bind_addr = if resolver.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind_addr)?;
    socket.set_read_timeout(Some(DNS_TIMEOUT))?;
    socket.set_write_timeout(Some(DNS_TIMEOUT))?;
    socket.connect(resolver)?;
    socket.send(query)?;

    let mut response = vec![0u8; MAX_DNS_PACKET_SIZE];
    let len = socket.recv(&mut response)?;
    response.truncate(len);
    Ok(response)
}

fn parse_question(packet: &[u8]) -> Result<DnsQuestion, ()> {
    if packet.len() < DNS_HEADER_LEN || read_u16(packet, DNS_QDCOUNT_OFFSET)? != DNS_ONE_QUESTION {
        return Err(());
    }

    let (name, after_name) = read_name(packet, DNS_HEADER_LEN)?;
    if after_name + DNS_QUESTION_FIXED_LEN > packet.len() {
        return Err(());
    }

    Ok(DnsQuestion {
        name,
        end: after_name + DNS_QUESTION_FIXED_LEN,
    })
}

fn parse_answer_ips(packet: &[u8]) -> Result<Vec<(IpAddr, u32)>, ()> {
    if packet.len() < DNS_HEADER_LEN {
        return Err(());
    }

    let qdcount = read_u16(packet, DNS_QDCOUNT_OFFSET)? as usize;
    let ancount = read_u16(packet, DNS_ANCOUNT_OFFSET)? as usize;
    let mut offset = DNS_HEADER_LEN;

    for _ in 0..qdcount {
        let (_, after_name) = read_name(packet, offset)?;
        if after_name + DNS_QUESTION_FIXED_LEN > packet.len() {
            return Err(());
        }
        offset = after_name + DNS_QUESTION_FIXED_LEN;
    }

    let mut ips = Vec::new();
    for _ in 0..ancount {
        let (_, after_name) = read_name(packet, offset)?;
        if after_name + DNS_RR_FIXED_LEN > packet.len() {
            return Err(());
        }
        offset = after_name;

        let rr_type = read_u16(packet, offset + DNS_RR_TYPE_OFFSET)?;
        let class = read_u16(packet, offset + DNS_RR_CLASS_OFFSET)?;
        let ttl = read_u32(packet, offset + DNS_RR_TTL_OFFSET)?;
        let rdlen = read_u16(packet, offset + DNS_RR_RDLEN_OFFSET)? as usize;
        offset += DNS_RR_FIXED_LEN;

        if offset + rdlen > packet.len() {
            return Err(());
        }

        if class == DNS_CLASS_IN && rr_type == DNS_TYPE_A && rdlen == DNS_A_RDATA_LEN {
            ips.push((
                IpAddr::V4(Ipv4Addr::new(
                    packet[offset],
                    packet[offset + 1],
                    packet[offset + 2],
                    packet[offset + 3],
                )),
                ttl,
            ));
        } else if class == DNS_CLASS_IN && rr_type == DNS_TYPE_AAAA && rdlen == DNS_AAAA_RDATA_LEN {
            ips.push((
                IpAddr::V6(Ipv6Addr::from(
                    <[u8; DNS_AAAA_RDATA_LEN]>::try_from(
                        &packet[offset..offset + DNS_AAAA_RDATA_LEN],
                    )
                    .map_err(|_| ())?,
                )),
                ttl,
            ));
        }

        offset += rdlen;
    }

    Ok(ips)
}

fn build_error_response(query: &[u8], rcode: u16) -> Vec<u8> {
    let id = if query.len() >= DNS_ID_LEN {
        &query[..DNS_ID_LEN]
    } else {
        &[0, 0]
    };
    let req_flags = if query.len() >= DNS_FLAGS_OFFSET + DNS_U16_LEN {
        read_u16(query, DNS_FLAGS_OFFSET).unwrap_or(0)
    } else {
        0
    };
    let flags = DNS_FLAG_RESPONSE
        | (req_flags & DNS_FLAG_RECURSION_DESIRED)
        | DNS_FLAG_RECURSION_AVAILABLE
        | (rcode & DNS_RCODE_MASK);

    let question = parse_question(query).ok();
    let mut response =
        Vec::with_capacity(question.as_ref().map(|q| q.end).unwrap_or(DNS_HEADER_LEN));
    response.extend_from_slice(id);
    response.extend_from_slice(&flags.to_be_bytes());
    if let Some(question) = question {
        response.extend_from_slice(&DNS_ONE_QUESTION.to_be_bytes());
        response.extend_from_slice(&DNS_COUNT_ZERO.to_be_bytes());
        response.extend_from_slice(&DNS_COUNT_ZERO.to_be_bytes());
        response.extend_from_slice(&DNS_COUNT_ZERO.to_be_bytes());
        response.extend_from_slice(&query[DNS_HEADER_LEN..question.end]);
    } else {
        response.extend_from_slice(&DNS_COUNT_ZERO.to_be_bytes());
        response.extend_from_slice(&DNS_COUNT_ZERO.to_be_bytes());
        response.extend_from_slice(&DNS_COUNT_ZERO.to_be_bytes());
        response.extend_from_slice(&DNS_COUNT_ZERO.to_be_bytes());
    }
    response
}

fn read_name(packet: &[u8], offset: usize) -> Result<(String, usize), ()> {
    let mut labels = Vec::new();
    let mut pos = offset;
    let mut next_offset = offset;
    let mut jumped = false;
    let mut jumps = 0;

    loop {
        if pos >= packet.len() {
            return Err(());
        }

        let len = packet[pos];
        if len & DNS_POINTER_MASK == DNS_POINTER_TAG {
            if pos + 1 >= packet.len() {
                return Err(());
            }
            let pointer =
                (((len & DNS_POINTER_OFFSET_MASK) as usize) << 8) | packet[pos + 1] as usize;
            if pointer >= packet.len() {
                return Err(());
            }
            if !jumped {
                next_offset = pos + DNS_U16_LEN;
            }
            pos = pointer;
            jumped = true;
            jumps += 1;
            if jumps > DNS_MAX_COMPRESSION_JUMPS {
                return Err(());
            }
            continue;
        }

        if len & DNS_POINTER_MASK != 0 {
            return Err(());
        }

        pos += 1;
        if len == 0 {
            if !jumped {
                next_offset = pos;
            }
            break;
        }

        let len = len as usize;
        if len > DNS_MAX_LABEL_LEN || pos + len > packet.len() {
            return Err(());
        }

        let label = std::str::from_utf8(&packet[pos..pos + len]).map_err(|_| ())?;
        labels.push(label.to_ascii_lowercase());
        pos += len;
        if !jumped {
            next_offset = pos;
        }
    }

    Ok((labels.join("."), next_offset))
}

fn read_u16(buf: &[u8], offset: usize) -> Result<u16, ()> {
    let bytes = buf.get(offset..offset + DNS_U16_LEN).ok_or(())?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(buf: &[u8], offset: usize) -> Result<u32, ()> {
    let bytes = buf.get(offset..offset + DNS_U32_LEN).ok_or(())?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::{SockaddrIn, SockaddrLike};

    fn query_for(name: &str) -> Vec<u8> {
        let mut query = vec![
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        for label in name.split('.') {
            query.push(label.len() as u8);
            query.extend_from_slice(label.as_bytes());
        }
        query.push(0);
        query.extend_from_slice(&1u16.to_be_bytes());
        query.extend_from_slice(&1u16.to_be_bytes());
        query
    }

    fn response_with_a(name: &str, ip: [u8; 4], ttl: u32) -> Vec<u8> {
        let mut response = query_for(name);
        response[2] = 0x81;
        response[3] = 0x80;
        response[6] = 0x00;
        response[7] = 0x01;
        response.extend_from_slice(&[0xc0, 0x0c]);
        response.extend_from_slice(&1u16.to_be_bytes());
        response.extend_from_slice(&1u16.to_be_bytes());
        response.extend_from_slice(&ttl.to_be_bytes());
        response.extend_from_slice(&4u16.to_be_bytes());
        response.extend_from_slice(&ip);
        response
    }

    fn sockaddr_v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SockaddrStorage {
        let sa = SockaddrIn::new(a, b, c, d, port);
        unsafe { SockaddrStorage::from_raw(sa.as_ptr(), Some(sa.len())).unwrap() }
    }

    #[test]
    fn hostname_matching_allows_exact_and_subdomain_only() {
        let policy = EgressPolicy::new(None, Some(vec!["Example.COM.".to_string()])).unwrap();

        assert!(policy.is_hostname_allowed("example.com"));
        assert!(policy.is_hostname_allowed("api.example.com"));
        assert!(!policy.is_hostname_allowed("badexample.com"));
        assert!(!policy.is_hostname_allowed("example.org"));
    }

    #[test]
    fn empty_allow_list_blocks_all_hostnames() {
        let policy = EgressPolicy::new(None, Some(vec![])).unwrap();
        assert!(!policy.is_hostname_allowed("example.com"));
    }

    #[test]
    fn parses_dns_query_name() {
        let question = parse_question(&query_for("api.example.com")).unwrap();

        assert_eq!(question.name, "api.example.com");
    }

    #[test]
    fn builds_nxdomain_with_original_question() {
        let query = query_for("blocked.example.com");
        let response = build_error_response(&query, DNS_RCODE_NXDOMAIN);

        assert_eq!(&response[..DNS_ID_LEN], &query[..DNS_ID_LEN]);
        assert_eq!(
            read_u16(&response, DNS_QDCOUNT_OFFSET).unwrap(),
            DNS_ONE_QUESTION
        );
        assert_eq!(
            read_u16(&response, DNS_ANCOUNT_OFFSET).unwrap(),
            DNS_COUNT_ZERO
        );
        assert_eq!(
            read_u16(&response, DNS_FLAGS_OFFSET).unwrap() & DNS_RCODE_MASK,
            DNS_RCODE_NXDOMAIN
        );
        assert_eq!(&response[DNS_HEADER_LEN..], &query[DNS_HEADER_LEN..]);
    }

    #[test]
    fn builds_servfail_for_malformed_query() {
        let response = build_error_response(&[0xab, 0xcd], DNS_RCODE_SERVFAIL);

        assert_eq!(&response[..2], &[0xab, 0xcd]);
        assert_eq!(
            read_u16(&response, DNS_FLAGS_OFFSET).unwrap() & DNS_RCODE_MASK,
            DNS_RCODE_SERVFAIL
        );
        assert_eq!(
            read_u16(&response, DNS_QDCOUNT_OFFSET).unwrap(),
            DNS_COUNT_ZERO
        );
    }

    #[test]
    fn parses_compressed_a_answer() {
        let response = response_with_a("allowed.example.com", [203, 0, 113, 5], 60);
        let ips = parse_answer_ips(&response).unwrap();

        assert_eq!(ips, vec![(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)), 60)]);
    }

    #[test]
    fn allowed_ips_allow_until_expired() {
        let mut policy = EgressPolicy::new(None, Some(vec!["example.com".to_string()])).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
        policy.learn_ips(vec![(ip, 1)]);

        assert!(policy
            .allowed_ips
            .get(&ip)
            .copied()
            .is_some_and(|expires_at| expires_at > Instant::now()));
    }

    #[test]
    fn dns_policy_denies_before_learn_and_allows_after_learn() {
        let mut policy = EgressPolicy::new(None, Some(vec!["example.com".to_string()])).unwrap();
        let addr = sockaddr_v4(203, 0, 113, 10, 443);

        assert!(!policy.is_addr_allowed(&addr));

        policy.learn_ips(vec![(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 60)]);

        assert!(policy.is_addr_allowed(&addr));
    }

    #[test]
    fn learn_ips_prunes_expired_ips() {
        let mut policy = EgressPolicy::new(None, Some(vec!["example.com".to_string()])).unwrap();
        let expired_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 22));
        let learned_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 23));
        let now = Instant::now();

        policy
            .allowed_ips
            .insert(expired_ip, now - Duration::from_secs(1));
        policy
            .allowed_ips
            .insert(learned_ip, now + Duration::from_secs(60));

        policy.learn_ips(vec![]);

        assert!(!policy.is_addr_allowed(&sockaddr_v4(203, 0, 113, 22, 443)));
        assert!(policy.is_addr_allowed(&sockaddr_v4(203, 0, 113, 23, 443)));
    }

    #[test]
    fn learn_ips_keeps_later_expiration() {
        let mut policy = EgressPolicy::new(None, Some(vec!["example.com".to_string()])).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 24));

        policy.learn_ips(vec![(ip, 60)]);
        let first_expiration = policy.allowed_ips[&ip];

        policy.learn_ips(vec![(ip, 1)]);
        assert_eq!(policy.allowed_ips[&ip], first_expiration);

        policy.learn_ips(vec![(ip, 120)]);
        assert!(policy.allowed_ips[&ip] > first_expiration);
    }

    #[test]
    fn resolve_hosts_to_ips_returns_ttl_bearing_entries() {
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 25));
        let ips = resolve_hosts_to_ips(&["203.0.113.25".to_string()], &[]);

        assert_eq!(ips, vec![(ip, MAX_LEARNED_TTL as u32)]);
    }

    #[test]
    fn allowed_cidrs_still_allow_with_dns_filter_enabled() {
        let policy = EgressPolicy::new(
            Some(vec![(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 8)]),
            Some(vec!["example.com".to_string()]),
        )
        .unwrap();

        assert!(policy.is_addr_allowed(&sockaddr_v4(10, 1, 2, 3, 443)));
        assert!(!policy.is_addr_allowed(&sockaddr_v4(11, 1, 2, 3, 443)));
    }

    #[test]
    fn dns_filter_intercepts_udp_port_53_only() {
        let policy = EgressPolicy::new(None, Some(vec!["example.com".to_string()])).unwrap();

        assert!(policy.should_intercept_dns(&sockaddr_v4(1, 1, 1, 1, 53)));
        assert!(!policy.should_intercept_dns(&sockaddr_v4(1, 1, 1, 1, 853)));
    }

    #[test]
    fn blocked_query_returns_nxdomain_without_learning_ips() {
        let policy = Arc::new(RwLock::new(
            EgressPolicy::new(None, Some(vec!["allowed.example.com".to_string()])).unwrap(),
        ));
        let response = handle_dns_query(
            &policy,
            &query_for("blocked.example.com"),
            &sockaddr_v4(127, 0, 0, 1, 53),
        );

        assert_eq!(
            read_u16(&response, DNS_FLAGS_OFFSET).unwrap() & DNS_RCODE_MASK,
            DNS_RCODE_NXDOMAIN
        );
        assert!(policy.read().unwrap().allowed_ips.is_empty());
    }
}
