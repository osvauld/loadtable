//! The `[peers]` section of the machine policy: which machines this daemon forwards to and
//! accepts jobs from.
//!
//! ```toml
//! [peers]
//! bind = "192.168.1.5:7843"      # LAN address this daemon listens on (never 0.0.0.0)
//!
//! [peers.nixos]                  # the name placement and `--peer` use
//! node_id = "ab12..."            # the peer's transport identity (required)
//! addr = "192.168.1.7:7843"      # where to reach it on the LAN (optional)
//! allow = true                   # may this peer send jobs here? (default true)
//! ```
//!
//! Parsing is strict like the rest of the policy: an unknown key, a wrong type, a bad name, a
//! duplicate node id or an unspecified bind refuses the whole file, naming each field. Peers are
//! read at daemon start; changing them needs a restart.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use tasks_peer::PeerId;

/// One peer machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSpec {
    /// The name placement, `--peer` and the ledger use.
    pub name: String,
    /// The peer's transport identity.
    pub node_id: PeerId,
    /// Where to reach it, when it has a fixed LAN address.
    pub addr: Option<SocketAddr>,
    /// True when this peer may submit jobs to this daemon (the receiving allowlist). A peer with
    /// `allow = false` is a forward target only.
    pub allow: bool,
}

/// Every configured peer plus this daemon's own listen address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerConfig {
    /// The LAN address to listen on; peers can only reach this daemon when it is set.
    pub bind: Option<SocketAddr>,
    /// The peers, in name order.
    pub peers: Vec<PeerSpec>,
}

impl PeerConfig {
    /// The identities allowed to submit jobs here.
    pub fn allowlist(&self) -> Vec<PeerId> {
        self.peers
            .iter()
            .filter(|p| p.allow)
            .map(|p| p.node_id.clone())
            .collect()
    }

    /// The peer called `name`.
    pub fn by_name(&self, name: &str) -> Option<&PeerSpec> {
        self.peers.iter().find(|p| p.name == name)
    }

    /// The peer with this identity.
    pub fn by_node(&self, id: &PeerId) -> Option<&PeerSpec> {
        self.peers.iter().find(|p| &p.node_id == id)
    }

    /// Parses the `peers` table. Returns the config (whatever parsed) and every problem found.
    pub fn parse(value: &toml::Value) -> (PeerConfig, Vec<String>) {
        let mut cfg = PeerConfig::default();
        let mut problems = Vec::new();
        let Some(table) = value.as_table() else {
            problems.push("peers: must be a table".to_string());
            return (cfg, problems);
        };
        let mut nodes = BTreeSet::new();
        for (key, v) in table {
            if key == "bind" {
                match v.as_str().map(str::parse::<SocketAddr>) {
                    Some(Ok(a)) if a.ip().is_unspecified() => problems.push(format!(
                        "peers.bind: {a} is unspecified; name the LAN interface address"
                    )),
                    Some(Ok(a)) => cfg.bind = Some(a),
                    Some(Err(e)) => problems.push(format!("peers.bind: not an address:port ({e})")),
                    None => problems.push("peers.bind: must be a string".to_string()),
                }
                continue;
            }
            let Some(t) = v.as_table() else {
                problems.push(format!("peers.{key}: unknown field (peers are tables)"));
                continue;
            };
            if !valid_name(key) {
                problems.push(format!(
                    "peers.{key}: a peer name is 1 to 64 of letters, digits, `.`, `-` or `_`"
                ));
                continue;
            }
            let mut node = None;
            let mut addr = None;
            let mut allow = true;
            for (k, field) in t {
                match k.as_str() {
                    "node_id" => match field.as_str().map(PeerId::new) {
                        Some(Ok(id)) => node = Some(id),
                        Some(Err(e)) => problems.push(format!("peers.{key}.node_id: {e}")),
                        None => problems.push(format!("peers.{key}.node_id: must be a string")),
                    },
                    "addr" => match field.as_str().map(str::parse::<SocketAddr>) {
                        Some(Ok(a)) => addr = Some(a),
                        Some(Err(e)) => {
                            problems.push(format!("peers.{key}.addr: not an address:port ({e})"));
                        }
                        None => problems.push(format!("peers.{key}.addr: must be a string")),
                    },
                    "allow" => match field.as_bool() {
                        Some(b) => allow = b,
                        None => problems.push(format!("peers.{key}.allow: must be true or false")),
                    },
                    other => problems.push(format!("peers.{key}.{other}: unknown field")),
                }
            }
            match node {
                Some(id) => {
                    if !nodes.insert(id.clone()) {
                        problems.push(format!("peers.{key}.node_id: {id} is already used"));
                    }
                    cfg.peers.push(PeerSpec {
                        name: key.clone(),
                        node_id: id,
                        addr,
                        allow,
                    });
                }
                None if !t.contains_key("node_id") => {
                    problems.push(format!("peers.{key}.node_id: required"));
                }
                None => {}
            }
        }
        (cfg, problems)
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The commented sample block for the policy sample file. Lines start with `#>` so the sample
/// test that uncomments every `# key = value` default leaves them alone; remove the `#>` to use.
pub const SAMPLE: &str = "\n# Peers (read at start; restart to change). Remove the leading `#>` to use.\n\
#> [peers]\n\
#> bind = \"192.168.1.5:7843\"\n\
#> [peers.nixos]\n\
#> node_id = \"<the peer's node id>\"\n\
#> addr = \"192.168.1.7:7843\"\n\
#> allow = true\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (PeerConfig, Vec<String>) {
        let v: toml::Value = toml::from_str(text).unwrap();
        PeerConfig::parse(v.get("peers").unwrap())
    }

    #[test]
    fn a_complete_section_parses_with_defaults() {
        let (c, p) = parse(
            "[peers]\nbind = \"192.168.1.5:7843\"\n[peers.nixos]\nnode_id = \"abc\"\naddr = \"192.168.1.7:7843\"\n[peers.mac]\nnode_id = \"def\"\nallow = false\n",
        );
        assert!(p.is_empty(), "{p:?}");
        assert_eq!(c.bind, Some("192.168.1.5:7843".parse().unwrap()));
        assert_eq!(c.peers.len(), 2);
        let n = c.by_name("nixos").unwrap();
        assert!(n.allow && n.addr.is_some());
        assert_eq!(c.allowlist(), vec![PeerId::new("abc").unwrap()]);
        assert!(c.by_node(&PeerId::new("def").unwrap()).is_some());
    }

    #[test]
    fn missing_and_bad_values_are_refused_naming_each_field() {
        let (_, p) = parse(
            "[peers]\nbind = \"0.0.0.0:1\"\nstray = 3\n[peers.a]\naddr = \"nowhere\"\nallow = \"yes\"\nextra = 1\n[peers.\"bad name\"]\nnode_id = \"x\"\n[peers.b]\nnode_id = 5\n[peers.c]\nnode_id = \"dup\"\n[peers.d]\nnode_id = \"dup\"\n",
        );
        let all = p.join("\n");
        for needle in [
            "peers.bind: 0.0.0.0:1 is unspecified",
            "peers.stray: unknown field",
            "peers.a.node_id: required",
            "peers.a.addr: not an address:port",
            "peers.a.allow: must be true or false",
            "peers.a.extra: unknown field",
            "peers.bad name",
            "peers.b.node_id: must be a string",
            "already used",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in\n{all}");
        }
    }

    #[test]
    fn peers_must_be_a_table() {
        let v: toml::Value = toml::from_str("peers = 3").unwrap();
        let (_, p) = PeerConfig::parse(v.get("peers").unwrap());
        assert!(p[0].contains("must be a table"));
    }
}
