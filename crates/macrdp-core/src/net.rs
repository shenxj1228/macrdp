//! Local network address discovery for the UI.
//!
//! The server can be reached over the LAN (`en*`) or over a VPN tunnel (`utun*`), and a
//! VPN-assigned address may change on every connection. The main window lists every usable
//! IPv4 address so the user can copy the one that works from the client's network.

use std::ffi::CStr;
use std::net::Ipv4Addr;

use serde::Serialize;

/// How an address is reached. Used for grouping and labelling in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AddressKind {
    Lan,
    Vpn,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalAddress {
    pub interface: String,
    pub address: String,
    pub kind: AddressKind,
}

fn classify(interface: &str) -> AddressKind {
    if ["utun", "ppp", "ipsec", "tun", "tap", "gpd"]
        .iter()
        .any(|prefix| interface.starts_with(prefix))
    {
        AddressKind::Vpn
    } else if ["en", "eth", "bridge"]
        .iter()
        .any(|prefix| interface.starts_with(prefix))
    {
        AddressKind::Lan
    } else {
        AddressKind::Other
    }
}

fn kind_order(kind: AddressKind) -> u8 {
    match kind {
        AddressKind::Lan => 0,
        AddressKind::Vpn => 1,
        AddressKind::Other => 2,
    }
}

/// All active, non-loopback IPv4 addresses, LAN first and VPN second.
///
/// Link-local (169.254/16) and loopback addresses are omitted: a client cannot use them.
pub fn local_addresses() -> Vec<LocalAddress> {
    let mut addresses = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a linked list, released with freeifaddrs below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return addresses;
    }

    let mut current = head;
    while !current.is_null() {
        // SAFETY: `current` is a node of the list returned by getifaddrs.
        let ifa = unsafe { &*current };
        current = ifa.ifa_next;

        if ifa.ifa_addr.is_null()
            || (ifa.ifa_flags & libc::IFF_UP as libc::c_uint) == 0
            || (ifa.ifa_flags & libc::IFF_LOOPBACK as libc::c_uint) != 0
        {
            continue;
        }
        // SAFETY: ifa_addr points to a sockaddr; its family selects the concrete type.
        if unsafe { (*ifa.ifa_addr).sa_family } != libc::AF_INET as libc::sa_family_t {
            continue;
        }

        // SAFETY: an AF_INET sockaddr is a sockaddr_in.
        let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
        // s_addr holds the address in network byte order; its in-memory bytes are the
        // octets in order, which is what Ipv4Addr::from expects.
        let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
        if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
            continue;
        }

        // SAFETY: ifa_name is a NUL-terminated string owned by the list.
        let interface = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy();
        let kind = classify(&interface);
        let address = LocalAddress {
            interface: interface.into_owned(),
            address: ip.to_string(),
            kind,
        };
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }

    // SAFETY: releases the list allocated by getifaddrs.
    unsafe { libc::freeifaddrs(head) };

    addresses.sort_by(|a, b| {
        kind_order(a.kind)
            .cmp(&kind_order(b.kind))
            .then_with(|| a.interface.cmp(&b.interface))
            .then_with(|| a.address.cmp(&b.address))
    });
    addresses
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_interfaces() {
        assert_eq!(classify("en0"), AddressKind::Lan);
        assert_eq!(classify("bridge0"), AddressKind::Lan);
        assert_eq!(classify("utun7"), AddressKind::Vpn);
        assert_eq!(classify("ppp0"), AddressKind::Vpn);
        assert_eq!(classify("awdl0"), AddressKind::Other);
    }

    #[test]
    fn skips_unusable_addresses() {
        for address in local_addresses() {
            let ip: Ipv4Addr = address.address.parse().expect("valid IPv4 address");
            assert!(!ip.is_loopback());
            assert!(!ip.is_link_local());
            assert!(!ip.is_unspecified());
        }
    }
}
