//! The `[peers]` section of the machine policy, and the peers a forwarder dials.
//!
//! Who may talk to this machine is no longer configured here: a peer exists because a ticket was
//! redeemed (`loadtable pair`), and what it may do is a grant in the ledger. The policy keeps only
//! transport choices:
//!
//! ```toml
//! [peers]
//! relay = "default"              # "default" (iroh's public relays), "off", or a relay URL
//! bind = "192.168.1.5:7843"      # optional fixed UDP socket (never 0.0.0.0)
//! ```
//!
//! Parsing is strict like the rest of the policy. The old per-peer tables (`node_id`, `addr`,
//! `allow`) are refused with a pointer to pairing. Settings are read at daemon start.

use std::net::SocketAddr;
use tasks_peer::PeerId;

/// A paired machine a forwarder may dial: learned from pairing, never from configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSpec {
    /// The name placement, `--peer` and the ledger use.
    pub name: String,
    /// The peer's node id (its device key).
    pub node_id: PeerId,
    /// The peer's DID.
    pub did: String,
}

/// Which relays the endpoint uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RelaySetting {
    /// iroh's public default relays, with discovery (design D2).
    #[default]
    Default,
    /// No relay and no discovery: direct addresses only.
    Off,
    /// A self-hosted relay.
    Custom(String),
}

/// Transport choices from the policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerConfig {
    /// A fixed UDP socket to bind, when set.
    pub bind: Option<SocketAddr>,
    /// Relay choice.
    pub relay: RelaySetting,
}

impl PeerConfig {
    /// Parses the `peers` table. Returns the config (whatever parsed) and every problem found.
    pub fn parse(value: &toml::Value) -> (PeerConfig, Vec<String>) {
        let mut cfg = PeerConfig::default();
        let mut problems = Vec::new();
        let Some(table) = value.as_table() else {
            problems.push("peers: must be a table".to_string());
            return (cfg, problems);
        };
        for (key, v) in table {
            match key.as_str() {
                "bind" => match v.as_str().map(str::parse::<SocketAddr>) {
                    Some(Ok(a)) if a.ip().is_unspecified() => problems.push(format!(
                        "peers.bind: {a} is unspecified; name the interface address"
                    )),
                    Some(Ok(a)) => cfg.bind = Some(a),
                    Some(Err(e)) => problems.push(format!("peers.bind: not an address:port ({e})")),
                    None => problems.push("peers.bind: must be a string".to_string()),
                },
                "relay" => match v.as_str() {
                    Some("default") => cfg.relay = RelaySetting::Default,
                    Some("off") => cfg.relay = RelaySetting::Off,
                    Some(url) if url.starts_with("https://") && url.len() <= 255 => {
                        cfg.relay = RelaySetting::Custom(url.to_string());
                    }
                    Some(other) => problems.push(format!(
                        "peers.relay: {other:?} is not \"default\", \"off\" or an https:// relay URL"
                    )),
                    None => problems.push("peers.relay: must be a string".to_string()),
                },
                other if v.is_table() => problems.push(format!(
                    "peers.{other}: peers are no longer configured here; pair with \
                     `loadtable pair invite` and `loadtable pair accept`"
                )),
                other => problems.push(format!("peers.{other}: unknown field")),
            }
        }
        (cfg, problems)
    }
}

/// The commented sample block for the policy sample file. Lines start with `#>` so the sample
/// test that uncomments every `# key = value` default leaves them alone; remove the `#>` to use.
pub const SAMPLE: &str = "\n# Peers are paired, not configured (`loadtable pair invite` / `pair accept`).\n\
# Transport choices (read at start; restart to change). Remove the leading `#>` to use.\n\
#> [peers]\n\
#> relay = \"default\"\n\
#> bind = \"192.168.1.5:7843\"\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (PeerConfig, Vec<String>) {
        let v: toml::Value = toml::from_str(text).unwrap();
        PeerConfig::parse(v.get("peers").unwrap())
    }

    // R-257
    #[test]
    fn r257_the_section_holds_only_transport_choices_with_public_relays_by_default() {
        let (c, p) = parse("[peers]\n");
        assert!(p.is_empty());
        assert_eq!(c, PeerConfig::default());
        assert_eq!(c.relay, RelaySetting::Default);
        let (c, p) = parse("[peers]\nrelay = \"off\"\nbind = \"192.168.1.5:7843\"\n");
        assert!(p.is_empty(), "{p:?}");
        assert_eq!(c.relay, RelaySetting::Off);
        assert_eq!(c.bind, Some("192.168.1.5:7843".parse().unwrap()));
        let (c, _) = parse("[peers]\nrelay = \"https://relay.example.org/\"\n");
        assert_eq!(
            c.relay,
            RelaySetting::Custom("https://relay.example.org/".into())
        );
    }

    // R-257
    #[test]
    fn r257_bad_values_and_the_old_per_peer_tables_are_refused_naming_each_field() {
        let (_, p) = parse(
            "[peers]\nbind = \"0.0.0.0:1\"\nrelay = \"http://x\"\nstray = 3\n[peers.nixos]\nnode_id = \"abc\"\n",
        );
        let all = p.join("\n");
        for needle in [
            "peers.bind: 0.0.0.0:1 is unspecified",
            "peers.relay:",
            "peers.stray: unknown field",
            "peers.nixos: peers are no longer configured here",
            "loadtable pair accept",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in\n{all}");
        }
        let v: toml::Value = toml::from_str("peers = 3").unwrap();
        let (_, p) = PeerConfig::parse(v.get("peers").unwrap());
        assert!(p[0].contains("must be a table"));
        let (_, p) = parse("[peers]\nbind = 5\nrelay = 5\n");
        assert_eq!(p.len(), 2);
    }
}
