//! Peer configuration (`~/.config/tasks/peer.toml` by default) and the allowlist built from it.
//!
//! ```toml
//! secret_key = "peer.key"            # relative to this file; `tasks peer init` creates it (0600)
//! bind = "192.168.1.10:7842"         # explicit interface address; 0.0.0.0/:: are refused
//! # checkouts = "~/offload/checkouts"  # where host peers' synced checkouts land
//! # store = "/path/blobs"             # iroh-blobs store (default: <state dir>/peer/blobs)
//! # reattach_timeout_s = 3600         # how long a forwarded job waits for a lost link
//! # connect_timeout_s = 10
//! # status_interval_s = 2
//! # heartbeat_s = 5                  # attach streams: heartbeat after this much silence
//! # link_stall_s = 20                 # no frame for this long = link lost (UNCERTAIN)
//!
//! [[peer]]
//! name = "worker"                    # [A-Za-z0-9._-]{1,64}
//! endpoint_id = "<64 hex or z32>"    # its `tasks peer id`
//! addr = "192.168.1.7:7842"          # where to dial it (needed for role worker/both)
//! role = "worker"                    # worker | host | both | observer
//! # approve = "auto"                 # inbound jobs from a host: auto | manual
//! ```
//!
//! Roles describe the PEER: `worker` runs jobs I forward (inbound it may only read my status);
//! `host` forwards jobs to me (status, jobs, source sync, artifact fetch); `both` is both;
//! `observer` may only read my status. Nobody else is admitted to anything.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use iroh::{EndpointAddr, PublicKey, RelayUrl, SecretKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Worker,
    Host,
    Both,
    Observer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approve {
    Auto,
    Manual,
}

/// What an inbound connection wants, by ALPN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Status,
    Jobs,
    Source,
    Artifact,
}

impl Role {
    /// Inbound permissions of a peer with this role.
    pub fn permits(self, need: Capability) -> bool {
        match need {
            Capability::Status => true,
            Capability::Jobs | Capability::Source | Capability::Artifact => {
                matches!(self, Role::Host | Role::Both)
            }
        }
    }

    /// May I forward jobs to a peer with this role?
    pub fn runs_my_jobs(self) -> bool {
        matches!(self, Role::Worker | Role::Both)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerEntry {
    pub name: String,
    #[serde(serialize_with = "ser_key")]
    pub endpoint: PublicKey,
    pub addr: Option<SocketAddr>,
    pub role: Role,
    pub approve: Approve,
}

fn ser_key<S: serde::Serializer>(key: &PublicKey, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&key.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPeer {
    name: String,
    endpoint_id: String,
    #[serde(default)]
    addr: Option<String>,
    role: Role,
    #[serde(default)]
    approve: Option<Approve>,
}

/// LAN remains the default. Relay mode is explicit local consent to contact the
/// configured HTTPS relay; discovery and port mapping remain disabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Lan,
    Relay,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    transport: Transport,
    #[serde(default)]
    relay_url: Option<String>,
    secret_key: String,
    bind: String,
    #[serde(default)]
    checkouts: Option<String>,
    #[serde(default)]
    store: Option<String>,
    #[serde(default)]
    reattach_timeout_s: Option<u64>,
    #[serde(default)]
    connect_timeout_s: Option<u64>,
    #[serde(default)]
    status_interval_s: Option<f64>,
    #[serde(default)]
    heartbeat_s: Option<f64>,
    #[serde(default)]
    link_stall_s: Option<f64>,
    #[serde(default, rename = "peer")]
    peers: Vec<RawPeer>,
}

#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub transport: Transport,
    /// A single operator-selected relay, shared by the paired endpoints. Never
    /// inferred from an untrusted inbound request or a discovery service.
    pub relay_url: Option<RelayUrl>,
    pub path: PathBuf,
    pub secret_key_path: PathBuf,
    pub bind: SocketAddr,
    pub checkouts: PathBuf,
    /// `None`: the embedder's default (`<state dir>/peer/blobs` for the daemon).
    pub store: Option<PathBuf>,
    pub reattach_timeout: Duration,
    pub connect_timeout: Duration,
    pub status_interval: Duration,
    /// Attach streams send a heartbeat after this much silence.
    pub heartbeat: Duration,
    /// No frame (heartbeats included) for this long = the link is lost.
    pub link_stall: Duration,
    pub peers: Vec<PeerEntry>,
}

pub const DEFAULT_REATTACH_TIMEOUT: Duration = Duration::from_secs(3600);
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Expands a leading `~/` with `home`; relative paths resolve against `base`.
pub fn expand(value: &str, base: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(rest) = value.strip_prefix("~/") {
        let home = home.ok_or_else(|| format!("{value}: HOME is not set"))?;
        return Ok(home.join(rest));
    }
    let path = Path::new(value);
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    })
}

impl PeerConfig {
    /// The config file the daemon and clients use: `TASKS_PEER_CONFIG`, else
    /// `$XDG_CONFIG_HOME/tasks/peer.toml`, else `~/.config/tasks/peer.toml`.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("TASKS_PEER_CONFIG") {
            return Some(path.into());
        }
        if let Some(xdg) =
            std::env::var_os("XDG_CONFIG_HOME").filter(|x| Path::new(x).is_absolute())
        {
            return Some(PathBuf::from(xdg).join("tasks/peer.toml"));
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/tasks/peer.toml"))
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::parse(&text, path, home.as_deref())
    }

    pub fn parse(text: &str, path: &Path, home: Option<&Path>) -> Result<Self, String> {
        let what = path.display();
        let raw: RawConfig = toml::from_str(text).map_err(|e| format!("{what}: {e}"))?;
        let base = path.parent().unwrap_or(Path::new("/"));
        let bind: SocketAddr = raw
            .bind
            .parse()
            .map_err(|e| format!("{what}: bind {:?}: {e}", raw.bind))?;
        if raw.transport == Transport::Lan && bind.ip().is_unspecified() {
            return Err(format!(
                "{what}: bind {bind} is unspecified; name the LAN interface address"
            ));
        }
        let relay_url = match (raw.transport, raw.relay_url) {
            (Transport::Lan, None) => None,
            (Transport::Lan, Some(_)) => {
                return Err(format!("{what}: relay_url requires transport = \"relay\""));
            }
            (Transport::Relay, None) => {
                return Err(format!(
                    "{what}: transport relay requires an explicit relay_url"
                ));
            }
            (Transport::Relay, Some(url)) => {
                let relay: RelayUrl = url.parse().map_err(|e| format!("{what}: relay_url: {e}"))?;
                if relay.scheme() != "https"
                    || relay.host_str().is_none()
                    || !relay.username().is_empty()
                    || relay.password().is_some()
                    || relay.query().is_some()
                    || relay.fragment().is_some()
                {
                    return Err(format!(
                        "{what}: relay_url must be HTTPS without credentials, query or fragment"
                    ));
                }
                Some(relay)
            }
        };
        let mut names = HashMap::new();
        let mut ids = HashMap::new();
        let mut peers = Vec::new();
        for p in raw.peers {
            if !valid_name(&p.name) {
                return Err(format!(
                    "{what}: peer name {:?} must match [A-Za-z0-9._-]{{1,64}}",
                    p.name
                ));
            }
            let endpoint = PublicKey::from_str(p.endpoint_id.trim())
                .map_err(|e| format!("{what}: peer {}: endpoint_id: {e}", p.name))?;
            let addr = match p.addr {
                Some(a) => Some(
                    a.parse::<SocketAddr>()
                        .map_err(|e| format!("{what}: peer {}: addr {a:?}: {e}", p.name))?,
                ),
                None => None,
            };
            if names.insert(p.name.clone(), ()).is_some() {
                return Err(format!("{what}: duplicate peer name {}", p.name));
            }
            if ids.insert(*endpoint.as_bytes(), ()).is_some() {
                return Err(format!("{what}: duplicate endpoint_id for peer {}", p.name));
            }
            peers.push(PeerEntry {
                name: p.name,
                endpoint,
                addr,
                role: p.role,
                approve: p.approve.unwrap_or(if raw.transport == Transport::Relay {
                    Approve::Manual
                } else {
                    Approve::Auto
                }),
            });
        }
        let checkouts = expand(
            raw.checkouts.as_deref().unwrap_or("~/offload/checkouts"),
            base,
            home,
        )?;
        let store = raw
            .store
            .as_deref()
            .map(|s| expand(s, base, home))
            .transpose()?;
        let status_interval = raw.status_interval_s.unwrap_or(2.0);
        if !(1.0..=3600.0).contains(&status_interval) {
            return Err(format!("{what}: status_interval_s must be 1..=3600"));
        }
        let heartbeat = raw.heartbeat_s.unwrap_or(5.0);
        let link_stall = raw.link_stall_s.unwrap_or(20.0);
        if !(0.2..=600.0).contains(&heartbeat)
            || link_stall < 2.0 * heartbeat
            || link_stall > 3600.0
        {
            return Err(format!(
                "{what}: need 0.2 <= heartbeat_s <= 600 and 2 x heartbeat_s <= link_stall_s <= 3600"
            ));
        }
        Ok(Self {
            transport: raw.transport,
            relay_url,
            path: path.to_owned(),
            secret_key_path: expand(&raw.secret_key, base, home)?,
            bind,
            checkouts,
            store,
            reattach_timeout: raw
                .reattach_timeout_s
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_REATTACH_TIMEOUT),
            connect_timeout: raw
                .connect_timeout_s
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            status_interval: Duration::from_secs_f64(status_interval),
            heartbeat: Duration::from_secs_f64(heartbeat),
            link_stall: Duration::from_secs_f64(link_stall),
            peers,
        })
    }

    pub fn peer(&self, name: &str) -> Option<&PeerEntry> {
        self.peers.iter().find(|p| p.name == name)
    }

    pub fn can_dial(&self, peer: &PeerEntry) -> bool {
        peer.addr.is_some() || self.relay_url.is_some()
    }

    /// Explicit addressing only: never publishes or resolves endpoint identities.
    /// In relay mode both operators must select the same relay URL.
    pub fn endpoint_addr(&self, peer: &PeerEntry) -> Result<EndpointAddr, String> {
        if !self.can_dial(peer) {
            return Err(format!(
                "peer {} has no addr or configured relay to dial",
                peer.name
            ));
        }
        let mut addr = EndpointAddr::new(peer.endpoint);
        if let Some(ip) = peer.addr {
            addr = addr.with_ip_addr(ip);
        }
        if let Some(relay) = &self.relay_url {
            addr = addr.with_relay_url(relay.clone());
        }
        Ok(addr)
    }

    pub fn allowlist(&self) -> Allowlist {
        Allowlist {
            peers: self.peers.clone(),
        }
    }

    pub fn secret_key(&self) -> Result<SecretKey, String> {
        load_secret_key(&self.secret_key_path)
    }
}

/// Reads a secret key file: 64 hex characters (optional trailing newline). Refuses a file that
/// is a symlink or readable by group/others.
pub fn load_secret_key(path: &Path) -> Result<SecretKey, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{}: not a regular file", path.display()));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{}: secret key must not be accessible by group/others (chmod 600)",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{}: expected 64 hex characters", path.display()));
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(SecretKey::from_bytes(&bytes))
}

/// Creates a new random key file (mode 0600, never overwrites). Returns the key.
pub fn create_secret_key(path: &Path) -> Result<SecretKey, String> {
    let key = SecretKey::generate();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    use std::io::Write;
    writeln!(file, "{}", crate::hex(&key.to_bytes()))
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(key)
}

/// Inbound admission: TLS-proven endpoint id must be configured with a role that permits the
/// capability. The only gate; account membership is not consulted.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    pub peers: Vec<PeerEntry>,
}

impl Allowlist {
    pub fn get(&self, id: &PublicKey) -> Option<&PeerEntry> {
        self.peers.iter().find(|p| p.endpoint == *id)
    }

    pub fn admit(&self, id: &PublicKey, need: Capability) -> Result<&PeerEntry, String> {
        match self.get(id) {
            None => Err("NotAllowlisted".into()),
            Some(p) if !p.role.permits(need) => {
                Err(format!("NotPermitted({:?} for role {:?})", need, p.role))
            }
            Some(p) => Ok(p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> String {
        SecretKey::generate().public().to_string()
    }

    #[test]
    fn parses_peers_roles_and_defaults() {
        let (a, b) = (key(), key());
        let text = format!(
            r#"secret_key = "peer.key"
bind = "127.0.0.1:7842"
[[peer]]
name = "worker"
endpoint_id = "{a}"
addr = "192.168.1.7:7842"
role = "worker"
[[peer]]
name = "laptop"
endpoint_id = "{b}"
role = "host"
approve = "manual"
"#
        );
        let c = PeerConfig::parse(
            &text,
            Path::new("/cfg/tasks/peer.toml"),
            Some(Path::new("/h")),
        )
        .unwrap();
        assert_eq!(c.secret_key_path, Path::new("/cfg/tasks/peer.key"));
        assert_eq!(c.checkouts, Path::new("/h/offload/checkouts"));
        assert_eq!(c.reattach_timeout, DEFAULT_REATTACH_TIMEOUT);
        let w = c.peer("worker").unwrap();
        assert!(w.role.runs_my_jobs() && !w.role.permits(Capability::Jobs));
        assert!(w.role.permits(Capability::Status));
        let l = c.peer("laptop").unwrap();
        assert_eq!(l.approve, Approve::Manual);
        let allow = c.allowlist();
        assert!(allow.admit(&l.endpoint, Capability::Source).is_ok());
        assert_eq!(
            allow.admit(&SecretKey::generate().public(), Capability::Status),
            Err("NotAllowlisted".into())
        );
        assert!(allow.admit(&w.endpoint, Capability::Jobs).is_err());
    }

    #[test]
    fn refuses_unspecified_bind_duplicates_and_bad_names() {
        let a = key();
        let base =
            |extra: &str, bind: &str| format!("secret_key = \"k\"\nbind = \"{bind}\"\n{extra}");
        let p = Path::new("/c/peer.toml");
        assert!(PeerConfig::parse(&base("", "0.0.0.0:1"), p, None).is_err());
        let dup = format!(
            "[[peer]]\nname=\"x\"\nendpoint_id=\"{a}\"\nrole=\"host\"\n[[peer]]\nname=\"y\"\nendpoint_id=\"{a}\"\nrole=\"host\"\n"
        );
        assert!(
            PeerConfig::parse(&base(&dup, "127.0.0.1:1"), p, None)
                .unwrap_err()
                .contains("duplicate endpoint_id")
        );
        let bad = format!("[[peer]]\nname=\"a b\"\nendpoint_id=\"{a}\"\nrole=\"host\"\n");
        assert!(PeerConfig::parse(&base(&bad, "127.0.0.1:1"), p, None).is_err());
        let unknown = "surprise = 1\n";
        assert!(PeerConfig::parse(&base(unknown, "127.0.0.1:1"), p, None).is_err());
    }

    #[test]
    fn relay_is_explicit_and_defaults_to_manual_approval() {
        let text = format!(
            "secret_key = \"k\"\nbind = \"0.0.0.0:0\"\ntransport = \"relay\"\nrelay_url = \"https://relay.example.invalid/\"\n[[peer]]\nname = \"mac\"\nendpoint_id = \"{}\"\nrole = \"both\"\n",
            key()
        );
        let c = PeerConfig::parse(&text, Path::new("/c/peer.toml"), Some(Path::new("/h"))).unwrap();
        assert_eq!(c.transport, Transport::Relay);
        let peer = c.peer("mac").unwrap();
        assert_eq!(peer.approve, Approve::Manual);
        assert!(peer.addr.is_none());
        assert!(c.can_dial(peer));
        let address = c.endpoint_addr(peer).unwrap();
        assert_eq!(address.id, peer.endpoint);
        assert_eq!(address.relay_urls().next(), c.relay_url.as_ref());
        assert_eq!(address.ip_addrs().count(), 0);
        assert!(
            c.allowlist()
                .admit(&SecretKey::generate().public(), Capability::Jobs)
                .is_err()
        );
        let auto = PeerConfig::parse(
            &(text + "approve = \"auto\"\n"),
            Path::new("/c/peer.toml"),
            Some(Path::new("/h")),
        )
        .unwrap();
        assert_eq!(auto.peer("mac").unwrap().approve, Approve::Auto);
    }

    #[test]
    fn lan_defaults_stay_private_and_relay_urls_are_restricted() {
        let parse = |extra: &str| {
            PeerConfig::parse(
                &format!("secret_key = \"k\"\nbind = \"127.0.0.1:0\"\n{extra}"),
                Path::new("/c/peer.toml"),
                Some(Path::new("/h")),
            )
        };
        let lan = parse("").unwrap();
        assert_eq!(lan.transport, Transport::Lan);
        assert!(lan.relay_url.is_none());
        assert!(parse("relay_url = \"https://relay.example.invalid/\"\n").is_err());
        assert!(parse("transport = \"relay\"\n").is_err());
        assert!(parse("transport = \"public\"\n").is_err());
        for url in [
            "http://localhost/",
            "https://user:pass@relay.example.invalid/",
            "https://relay.example.invalid/?token=secret",
            "https://relay.example.invalid/#fragment",
            "file:///tmp/relay",
        ] {
            assert!(
                parse(&format!("transport = \"relay\"\nrelay_url = \"{url}\"\n")).is_err(),
                "{url}"
            );
        }
        let peer_text = format!(
            "[[peer]]\nname = \"mac\"\nendpoint_id = \"{}\"\nrole = \"worker\"\n",
            key()
        );
        let c = parse(&peer_text).unwrap();
        assert!(!c.can_dial(c.peer("mac").unwrap()));
        assert!(c.endpoint_addr(c.peer("mac").unwrap()).is_err());
        assert_eq!(
            c.peer("mac").unwrap().approve,
            Approve::Auto,
            "legacy LAN semantics unchanged"
        );
    }

    #[test]
    fn secret_key_file_is_private_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("peer-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("peer.key");
        let key = create_secret_key(&path).unwrap();
        assert!(create_secret_key(&path).is_err(), "never overwrites");
        assert_eq!(load_secret_key(&path).unwrap().public(), key.public());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_secret_key(&path).unwrap_err().contains("chmod 600"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
