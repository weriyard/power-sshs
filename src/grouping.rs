//! Grouping of hosts by client prefix and colouring by environment slot.
//!
//! Host names follow `<client>-<role>-<slot>`, for example `zeta-web-b`.
//! The client is everything before the first `-`; the slot is everything
//! after the last `-`.

use crate::ssh::Host;

/// Environment slot taken from the last `-`-separated segment of a host name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// `-b`: non-production.
    B,
    /// `-g`: production.
    G,
    Other,
}

/// A tab of the host list. The order of [`Tab::ALL`] is the order on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// Hosts of the `dev` group.
    Dev,
    /// Hosts in slot `b`.
    B,
    /// Hosts in slot `g`.
    G,
    /// Every host.
    All,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Dev, Tab::B, Tab::G, Tab::All];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Tab::Dev => "devs",
            Tab::B => "blue",
            Tab::G => "green",
            Tab::All => "all",
        }
    }

    #[must_use]
    pub fn contains(self, name: &str) -> bool {
        match self {
            Tab::Dev => group_key(name).as_deref() == Some("dev"),
            Tab::B => slot(name) == Slot::B,
            Tab::G => slot(name) == Slot::G,
            Tab::All => true,
        }
    }

    #[must_use]
    pub fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    #[must_use]
    pub fn previous(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }

    #[must_use]
    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Returns the client group of a host name, lowercased, or `None` when the
/// name has no `-` and therefore no client prefix.
#[must_use]
pub fn group_key(name: &str) -> Option<String> {
    let (prefix, _) = name.split_once('-')?;
    if prefix.is_empty() {
        return None;
    }
    Some(prefix.to_lowercase())
}

/// Indentation of a host name under its group header.
pub const INDENT: &str = "  ";

/// The host name as shown in the table: indented when it sits under a group
/// header.
#[must_use]
pub fn display_name(name: &str) -> String {
    match group_key(name) {
        Some(_) => format!("{INDENT}{name}"),
        None => name.to_string(),
    }
}

#[must_use]
pub fn slot(name: &str) -> Slot {
    match name.rsplit_once('-').map(|(_, last)| last.to_lowercase()) {
        Some(s) if s == "b" => Slot::B,
        Some(s) if s == "g" => Slot::G,
        _ => Slot::Other,
    }
}

/// Reorders hosts so that each client group is contiguous.
///
/// Groups appear in the order of their first host; hosts keep their relative
/// order inside a group. Hosts without a group are kept together as one more
/// group, in the position of the first of them.
#[must_use]
pub fn group_hosts(hosts: Vec<Host>) -> Vec<Host> {
    let mut groups: Vec<(Option<String>, Vec<Host>)> = Vec::new();

    for host in hosts {
        let key = group_key(&host.name);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(host),
            None => groups.push((key, vec![host])),
        }
    }

    groups
        .into_iter()
        .flat_map(|(_, members)| members)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> Host {
        Host {
            name: name.to_string(),
            aliases: String::new(),
            user: None,
            destination: name.to_string(),
            port: None,
            proxy_command: None,
        }
    }

    fn names(hosts: &[Host]) -> Vec<&str> {
        hosts.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn test_group_key() {
        assert_eq!(group_key("zeta-web-b").as_deref(), Some("zeta"));
        assert_eq!(group_key("ACME-web-g").as_deref(), Some("acme"));
        assert_eq!(group_key("dev-web").as_deref(), Some("dev"));
        assert_eq!(group_key("bastion"), None);
        assert_eq!(group_key("-odd"), None);
    }

    #[test]
    fn test_tab_contains() {
        assert!(Tab::Dev.contains("dev-web"));
        assert!(Tab::Dev.contains("DEV-db"));
        assert!(!Tab::Dev.contains("devbox"));
        assert!(!Tab::Dev.contains("acme-web-b"));
        assert!(Tab::B.contains("acme-web-b"));
        assert!(!Tab::B.contains("zeta-web-g"));
        assert!(Tab::G.contains("zeta-web-g"));
        assert!(!Tab::G.contains("dev-web"));
        assert!(Tab::All.contains("nas"));
    }

    #[test]
    fn test_tab_order_wraps() {
        assert_eq!(Tab::Dev.next(), Tab::B);
        assert_eq!(Tab::B.next(), Tab::G);
        assert_eq!(Tab::G.next(), Tab::All);
        assert_eq!(Tab::All.next(), Tab::Dev);
        assert_eq!(Tab::Dev.previous(), Tab::All);
        assert_eq!(Tab::All.previous(), Tab::G);
    }

    #[test]
    fn test_display_name() {
        assert_eq!(display_name("zeta-web-b"), "  zeta-web-b");
        assert_eq!(display_name("bastion"), "bastion");
    }

    #[test]
    fn test_slot() {
        assert_eq!(slot("zeta-web-b"), Slot::B);
        assert_eq!(slot("zeta-web-g"), Slot::G);
        assert_eq!(slot("ZETA-WEB-G"), Slot::G);
        assert_eq!(slot("dev-web"), Slot::Other);
        assert_eq!(slot("b"), Slot::Other);
        assert_eq!(slot("acme-db"), Slot::Other);
    }

    #[test]
    fn test_group_hosts_keeps_first_appearance_order() {
        let hosts = vec![
            host("dev-web"),
            host("acme-web-b"),
            host("zeta-web-b"),
            host("acme-web-g"),
            host("bastion"),
            host("zeta-web-g"),
            host("dev-db"),
        ];

        assert_eq!(
            names(&group_hosts(hosts)),
            vec![
                "dev-web",
                "dev-db",
                "acme-web-b",
                "acme-web-g",
                "zeta-web-b",
                "zeta-web-g",
                "bastion",
            ]
        );
    }
}
