//! Whether this computer has a network route, without sending anything.
//!
//! [`route_to`] binds a UDP socket and connects it to each address. A UDP connect sends no packet:
//! it only asks the system to pick a local route, so [`Route::Selected`] means that a route exists
//! on this computer, not that the server can be reached. With no address, it checks the
//! documentation addresses `192.0.2.1` (RFC 5737) and `2001:db8::1` (RFC 3849); a route to either
//! means a default route exists.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// Whether the system has a route to an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The system has no route: the network is down or unplugged, or no interface reaches it.
    None,
    /// The system picked a local route. The server may still be unreachable beyond it.
    Selected,
    /// The check failed for another reason.
    Unknown,
}

/// The addresses checked when no address of the server is known.
pub const DEFAULT_ROUTE_PROBES: [SocketAddr; 2] = [
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 9),
    SocketAddr::new(std::net::IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)), 9),
];

/// Whether the system has a route to any of `addrs`, or to [`DEFAULT_ROUTE_PROBES`] when `addrs`
/// is empty: [`Route::Selected`] when it has one to any, [`Route::None`] when it has none to
/// every one, and [`Route::Unknown`] otherwise.
pub fn route_to(addrs: &[SocketAddr]) -> Route {
    let addrs = if addrs.is_empty() { &DEFAULT_ROUTE_PROBES[..] } else { addrs };
    combine(addrs.iter().map(|a| local_route(*a)))
}

/// The route to one address.
pub fn local_route(addr: SocketAddr) -> Route {
    let any: SocketAddr = if addr.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
    match UdpSocket::bind(any).and_then(|s| s.connect(addr)) {
        Ok(()) => Route::Selected,
        Err(e) => route_of(e.kind()),
    }
}

/// What a failed UDP connect says about the route.
fn route_of(kind: ErrorKind) -> Route {
    match kind {
        ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable | ErrorKind::NetworkDown | ErrorKind::AddrNotAvailable => Route::None,
        _ => Route::Unknown,
    }
}

/// The route of several checks: any `Selected` wins, then `Unknown`, then `None`.
fn combine(routes: impl Iterator<Item = Route>) -> Route {
    let mut out = Route::None;
    for r in routes {
        match r {
            Route::Selected => return Route::Selected,
            Route::Unknown => out = Route::Unknown,
            Route::None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loopback_address_has_a_route() {
        assert_eq!(local_route("127.0.0.1:9".parse().unwrap()), Route::Selected);
    }

    #[test]
    fn unreachable_errors_mean_no_route() {
        for kind in [ErrorKind::NetworkUnreachable, ErrorKind::HostUnreachable, ErrorKind::NetworkDown, ErrorKind::AddrNotAvailable] {
            assert_eq!(route_of(kind), Route::None, "{kind:?}");
        }
        for kind in [ErrorKind::PermissionDenied, ErrorKind::Other, ErrorKind::ConnectionRefused] {
            assert_eq!(route_of(kind), Route::Unknown, "{kind:?}");
        }
    }

    #[test]
    fn one_selected_route_is_enough() {
        use Route::*;
        assert_eq!(combine([None, Selected, Unknown].into_iter()), Selected);
        assert_eq!(combine([None, Unknown].into_iter()), Unknown);
        assert_eq!(combine([None, None].into_iter()), None);
        assert_eq!(combine(std::iter::empty()), None);
    }
}
