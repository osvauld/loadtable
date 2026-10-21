# Explicit relay transport (Mac preparation branch)

Status: opt-in implementation on `feature/macos-worker`; no internet, NAT or real
relay acceptance run yet. The macOS daemon port remains a separate gate. This
extends the LAN-only design in `peer-link.md`; it does not change existing LAN
configuration, default endpoint behavior, or the retained Tinline LAN adapter.

## Local policy and privacy

The existing default remains `transport = "lan"`: one explicit interface,
no relays, address lookup/discovery or port mapping. A relay URL in LAN mode is
refused rather than silently activating network access.

A machine owner may explicitly select `transport = "relay"` and one HTTPS
`relay_url`. The endpoint contacts that configured relay, not an inferred public
service. Both machines must select the same relay for this first version. The
relay carries encrypted traffic and can observe connection metadata; it does not
grant a peer authority. TLS endpoint IDs, signed sessions, local role allowlists,
job approval, cancellation, idempotency and artifact ownership still apply.
Address publication/lookup and port mapping remain disabled. Direct peer addresses
are optional in relay mode; a relay-only configuration does not promise direct
NAT traversal. No new public relay is provisioned or chosen by this branch.

Only HTTPS relay URLs without credentials, query strings or fragments are
accepted. This prevents putting URL credentials in status/logs and does not make
an arbitrary relay trustworthy. The URL remains local policy, never input accepted
from a remote workload. Changes require an explicitly coordinated idle restart;
no job or UI action silently restarts a busy daemon.

Inbound peers in relay mode default to `approve = "manual"`; LAN keeps its legacy
`auto` default for compatibility. Explicit `auto` is trusted arbitrary project
code execution as the worker account, not a sandbox or toolchain-setup permission.

## CLI (source branch, not installed release)

Do not run these against a shared installed daemon just to test the source.
Prepare the initial config before starting a new worker. Existing configs are
never replaced by `init`.

```sh
# EACH machine: the owner chooses and approves an existing compatible relay URL.
tasks peer init --relay https://YOUR-APPROVED-RELAY/
# Prints this machine's public endpoint ID, creates a private peer key.

# Linux controller, after verifying the Mac owner's endpoint ID out of band:
tasks peer add mac MAC_ENDPOINT_ID - --role worker

# Mac, after verifying the Linux controller's endpoint ID out of band:
tasks peer add linux LINUX_ENDPOINT_ID - --role host --approve manual
```

`-` means no fixed IP: use the relay already selected in the local config. It is
refused in LAN-only configs. There is no rendezvous by name: `mac` is only a local
alias. Confirm endpoint IDs through a trusted conversation; an unverified copied
ID does not establish whose machine it is. Invite expiry/account binding and a
one-click pairing UI are not implemented here.

Equivalent new-worker config:

```toml
secret_key = "peer.key"
bind = "0.0.0.0:0"
transport = "relay"
relay_url = "https://YOUR-APPROVED-RELAY/"

[[peer]]
name = "linux"
endpoint_id = "VERIFIED_CONTROLLER_ENDPOINT_ID"
role = "host"
approve = "manual"
```

Once a peer-capable daemon is running on both supported machines, the controller
interface remains `tasks run --peer mac --fetch REL -- COMMAND ...`. Forwarding,
status subscription, reconnect probes, cancellation, log search and fetch all use
the configured relay address as well as an optional explicit direct address.
`tasks peer list --json` exposes the effective `transport` and `relay_url`.

## Validation gates

Linux tests cover config validation, legacy defaults, relay default manual
approval, endpoint addressing and existing loopback peer semantics. They do not
prove a Mac worker or a public relay connection. Before offering this to a friend:

1. Native Apple Silicon worker build and local safety tests must pass.
2. Both owners explicitly consent to the chosen relay and verified peer IDs.
3. Run source/job/log/artifact transfer on the actual separate networks, no SSH.
4. Refuse an unknown endpoint; demonstrate manual approval and owner cancellation.
5. Interrupt the link during a job and reconnect without a duplicate submission.
6. Observe relay outage as unavailable/UNCERTAIN, never successful or auto-replayed.

The separate `lan_endpoint` function and its regression tests stay relay-free.
