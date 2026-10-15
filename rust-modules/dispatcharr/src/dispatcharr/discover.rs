//! **Finding a Dispatcharr on the local network**, so Setup can fill in the address rather than ask
//! for it (the owner's ask, 2026-10-15).
//!
//! Dispatcharr emulates an HDHomeRun, but on the owner's network it answered neither an SSDP
//! search nor HDHomeRun's own UDP discovery broadcast (measured 2026-10-15: probably a container
//! without host networking), so neither protocol can be relied on. What every install does answer
//! is its web port: this scans the television's own private IPv4 networks for TCP [`PORT`] and asks
//! each host that accepts what it is (`GET /api/core/version/`, which needs no credential, the
//! same probe sign-in starts with). Only private networks are scanned, only a subnet's worth of
//! hosts per interface, and nothing is sent to a host but that one request.
//!
//! BLOCKS for up to a few seconds; the application runs it on a worker.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::client::Server;

/// Dispatcharr's default web port.
pub const PORT: u16 = 9191;
/// How long one host may take to accept the port.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
/// Probes in flight at once: a /24 is scanned in about two connect timeouts.
const PARALLEL: usize = 64;
/// The widest prefix scanned whole. A larger network is scanned as the /24 around the TV's own
/// address: a home network is a /24, a dead address costs a whole [`CONNECT_TIMEOUT`], and a /22
/// per interface took 151 s on a build box with four bridge networks (2026-10-15).
const SCAN_PREFIX_MIN: u8 = 24;

/// A Dispatcharr that answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// What Setup's address field takes: `http://<ip>:9191`.
    pub address: String,
    /// The version it reported.
    pub version: String,
}

/// Every Dispatcharr on the television's networks, nearest address first.
pub fn scan() -> Vec<Found> {
    let mut hosts = Vec::new();
    for (ip, prefix) in local_networks() {
        for h in candidates(ip, prefix) {
            if !hosts.contains(&h) {
                hosts.push(h);
            }
        }
    }
    let open = open_hosts(&hosts, PORT);
    let mut found: Vec<Found> = open.into_iter().filter_map(|ip| identify(SocketAddr::new(IpAddr::V4(ip), PORT))).collect();
    found.sort_by(|a, b| a.address.cmp(&b.address));
    found
}

/// Ask `addr` whether it is a Dispatcharr.
pub fn identify(addr: SocketAddr) -> Option<Found> {
    let address = format!("http://{addr}");
    let server = Server::parse(&address).ok()?;
    let version = super::auth::version(&server).ok()?;
    Some(Found { address, version: version.version })
}

/// Whether `ip` is in a private range (RFC 1918): the only networks scanned.
fn private(ip: Ipv4Addr) -> bool {
    ip.is_private() && !ip.is_loopback() && !ip.is_link_local()
}

/// The hosts of the network `ip/prefix` to probe: every host address but `ip` itself, the network
/// and the broadcast address. A prefix wider than [`SCAN_PREFIX_MIN`] is narrowed to that around
/// `ip`. Empty for a network that is not private.
pub fn candidates(ip: Ipv4Addr, prefix: u8) -> Vec<Ipv4Addr> {
    if !private(ip) || prefix >= 31 {
        return Vec::new();
    }
    let prefix = prefix.max(SCAN_PREFIX_MIN);
    let mask = u32::MAX << (32 - u32::from(prefix));
    let net = u32::from(ip) & mask;
    let broadcast = net | !mask;
    ((net + 1)..broadcast).map(Ipv4Addr::from).filter(|&h| h != ip).collect()
}

/// The hosts among `hosts` that accept a TCP connection on `port`, [`PARALLEL`] at a time.
fn open_hosts(hosts: &[Ipv4Addr], port: u16) -> Vec<Ipv4Addr> {
    let queue = Arc::new(Mutex::new(hosts.to_vec()));
    let open = Arc::new(Mutex::new(Vec::new()));
    let workers: Vec<_> = (0..PARALLEL.min(hosts.len()))
        .filter_map(|_| {
            let (queue, open) = (Arc::clone(&queue), Arc::clone(&open));
            std::thread::Builder::new()
                .name("dispatcharr scan".into())
                .stack_size(64 * 1024)
                .spawn(move || loop {
                    let Some(ip) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop() else { return };
                    if TcpStream::connect_timeout(&SocketAddr::new(IpAddr::V4(ip), port), CONNECT_TIMEOUT).is_ok() {
                        open.lock().unwrap_or_else(|e| e.into_inner()).push(ip);
                    }
                })
                .ok()
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    let open = open.lock().unwrap_or_else(|e| e.into_inner()).clone();
    open
}

/// The television's IPv4 addresses and their prefixes, private networks only.
fn local_networks() -> Vec<(Ipv4Addr, u8)> {
    let mut out = Vec::new();
    // SAFETY: `getifaddrs` fills a list this function walks read-only and frees exactly once; every
    // pointer read is checked for null, and an address is read as `sockaddr_in` only when its family
    // says it is one.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null()
                && !ifa.ifa_netmask.is_null()
                && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET
            {
                let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let mask = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
                let prefix = u32::from_be(mask.sin_addr.s_addr).count_ones() as u8;
                if private(ip) {
                    out.push((ip, prefix));
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_subnet_is_every_other_host_on_it() {
        let me = Ipv4Addr::new(192, 168, 1, 50);
        let hosts = candidates(me, 24);
        assert_eq!(hosts.len(), 253, "254 hosts less this one");
        assert!(!hosts.contains(&me));
        assert_eq!(hosts.first(), Some(&Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(hosts.last(), Some(&Ipv4Addr::new(192, 168, 1, 254)), "not the broadcast address");
    }

    #[test]
    fn a_wide_network_is_scanned_near_the_tv_and_a_public_one_not_at_all() {
        let hosts = candidates(Ipv4Addr::new(10, 1, 2, 3), 8);
        assert_eq!(hosts.len(), 253, "the /24 around the TV");
        assert!(hosts.contains(&Ipv4Addr::new(10, 1, 2, 1)) && hosts.contains(&Ipv4Addr::new(10, 1, 2, 254)));
        assert!(candidates(Ipv4Addr::new(8, 8, 8, 8), 24).is_empty(), "never a public network");
        assert!(candidates(Ipv4Addr::new(127, 0, 0, 1), 8).is_empty());
        assert!(candidates(Ipv4Addr::new(192, 168, 1, 2), 31).is_empty(), "a point-to-point link has no one else");
    }

    #[test]
    fn a_dispatcharr_identifies_itself_and_a_port_that_is_closed_does_not() {
        let _g = plx_base::testlock::serial();
        if !super::super::test_double::curl_ready() {
            return;
        }
        let (server, _) = super::super::test_double::dispatcharr(None);
        let addr: SocketAddr = server.base().trim_start_matches("http://").parse().unwrap();
        let found = identify(addr).expect("the double answers the version probe");
        assert_eq!(found.address, format!("http://{addr}"));
        assert!(!found.version.is_empty());
        assert_eq!(open_hosts(&[Ipv4Addr::LOCALHOST], addr.port()), vec![Ipv4Addr::LOCALHOST]);
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(open_hosts(&[Ipv4Addr::LOCALHOST], closed).is_empty(), "a port nobody listens on");
    }
}
