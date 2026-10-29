use std::net::Ipv4Addr;

use if_addrs::{IfAddr, Interface as IfAddrsInterface};

use crate::settings::InterfaceChoice;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Interface {
    pub name: String,
    pub addr: Ipv4Addr,
}

/// An enumeration failure yields an empty list: the picker then offers
/// `Automatic` only, and a session can still start.
#[must_use]
pub fn enumerate() -> Vec<Interface> {
    if_addrs::get_if_addrs().map_or_else(|_error| Vec::new(), collect)
}

fn collect(interfaces: Vec<IfAddrsInterface>) -> Vec<Interface> {
    let mut listed: Vec<Interface> = interfaces
        .into_iter()
        .filter_map(|interface| match interface.addr {
            IfAddr::V4(v4) => (!v4.ip.is_loopback()).then_some(Interface {
                name: interface.name,
                addr: v4.ip,
            }),
            IfAddr::V6(_) => None,
        })
        .collect();
    listed.sort();
    listed
}

pub fn resolve(choice: &InterfaceChoice) -> Result<Ipv4Addr, String> {
    resolve_in(choice, &enumerate())
}

pub fn resolve_in(
    choice: &InterfaceChoice,
    interfaces: &[Interface],
) -> Result<Ipv4Addr, String> {
    match choice {
        InterfaceChoice::Auto => Ok(Ipv4Addr::UNSPECIFIED),
        InterfaceChoice::Named(name) => interfaces
            .iter()
            .find(|interface| interface.name == *name)
            .map(|interface| interface.addr)
            .ok_or_else(|| name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed() -> Vec<Interface> {
        vec![
            Interface {
                name: String::from("eth0"),
                addr: Ipv4Addr::new(192, 168, 1, 2),
            },
            Interface {
                name: String::from("wlan0"),
                addr: Ipv4Addr::new(10, 0, 0, 5),
            },
        ]
    }

    #[test]
    fn auto_resolves_to_every_interface() {
        assert_eq!(resolve(&InterfaceChoice::Auto), Ok(Ipv4Addr::UNSPECIFIED));
        assert_eq!(
            resolve_in(&InterfaceChoice::Auto, &listed()),
            Ok(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn a_named_interface_resolves_to_its_address() {
        let choice = InterfaceChoice::Named(String::from("wlan0"));
        assert_eq!(
            resolve_in(&choice, &listed()),
            Ok(Ipv4Addr::new(10, 0, 0, 5))
        );
    }

    #[test]
    fn a_missing_name_resolves_to_the_name() {
        let choice = InterfaceChoice::Named(String::from("vanish0"));
        assert_eq!(resolve_in(&choice, &listed()), Err(String::from("vanish0")));
    }

    #[test]
    fn enumeration_is_sorted_and_skips_loopback() {
        assert!(
            enumerate().windows(2).all(|pair| pair[0] <= pair[1]),
            "interfaces must be sorted by name"
        );
    }
}
