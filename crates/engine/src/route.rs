//! Whether this machine has any route off itself.
//!
//! The weakest network question there is, and deliberately so. Everything
//! stronger has a way of being wrong about *this* machine:
//!
//! * an ssh alias is not a hostname — `git@work-gitlab:x` resolves to nothing
//!   and clones fine;
//! * an `insteadOf` rewrite means the URL we read is not the URL git dials,
//!   and [`git_scylla_probe::config`] says so in its own header;
//! * a proxy, a jump host or a VPN answers for names that resolve to nothing
//!   here.
//!
//! Every one of those still needs a route, so "no route" is the one negative
//! that cannot be a false one. It is never read as a positive: a route is not
//! a promise that anything answers, only that trying is worth the subprocess.
//!
//! The addresses are reserved for documentation (RFC 5737, RFC 3849) and will
//! never be assigned, and a connected UDP socket sends nothing — the kernel
//! consults its routing table and that is all. No packet, no name lookup, no
//! third party, about a fifth of a millisecond.

use std::net::{SocketAddr, UdpSocket};

/// The engine's seam to the routing table, as [`Probe`] is its seam to the
/// filesystem: a test that needs a machine with no network must not need a
/// machine with no network.
///
/// [`Probe`]: git_scylla_probe::Probe
pub trait Route: std::fmt::Debug + Send + Sync {
    /// `false` only when the kernel has no route to a global address.
    fn exists(&self) -> bool;
}

/// TEST-NET-1 and the documentation prefix: reserved, never assigned, and
/// never contacted — only looked up.
const PROBES: [&str; 2] = ["192.0.2.1:9", "[2001:db8::1]:9"];

/// The real routing table.
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelRoutes;

impl Route for KernelRoutes {
    fn exists(&self) -> bool {
        // Either family is enough: a machine with only one of them is on a
        // network, and the plan is not the place to argue about which.
        PROBES.iter().any(|p| reachable_family(p))
    }
}

fn reachable_family(target: &str) -> bool {
    let Ok(addr) = target.parse::<SocketAddr>() else { return false };
    let bind: SocketAddr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse().unwrap();
    let Ok(socket) = UdpSocket::bind(bind) else { return false };
    socket.connect(addr).is_ok()
}

/// A routing table a test can set.
#[derive(Debug, Clone)]
pub struct FixedRoute(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl FixedRoute {
    pub fn new(exists: bool) -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(exists)))
    }

    /// Plug the machine back in, or pull it out, while the engine runs.
    pub fn set(&self, exists: bool) {
        self.0.store(exists, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Route for FixedRoute {
    fn exists(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_on_a_network_has_a_route() {
        // The only assertion that can be made without taking the network away:
        // this one runs somewhere, and somewhere has a route. The negative is
        // covered through `FixedRoute`, which is why the seam exists.
        assert!(KernelRoutes.exists());
    }

    #[test]
    fn the_lookup_costs_nothing_worth_measuring() {
        // It sits in front of every plan, so it has to be free. Generous
        // enough not to flake on a loaded machine; a lookup that started
        // resolving names or opening connections would blow straight past it.
        let start = std::time::Instant::now();
        for _ in 0..100 {
            KernelRoutes.exists();
        }
        let each = start.elapsed() / 100;
        assert!(each < std::time::Duration::from_millis(5), "{each:?} per check");
    }

    #[test]
    fn a_fixed_route_answers_what_it_was_told_to() {
        let route = FixedRoute::new(false);
        assert!(!route.exists());
        route.set(true);
        assert!(route.exists());
    }
}
