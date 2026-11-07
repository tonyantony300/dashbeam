# dashbeam-agent

The headless DashBeam node that AI agents and terminals drive. This crate (`dashbeam-agent`, lib name `agent`) holds all of the agent's behavior: identity, config, pairing policy, sending, receiving, tickets and the transfer registry. It has no UI, socket or MCP code. Those live in [`engine/cli`](../cli), which wraps one `AgentService` per data dir. For install and usage, read [`engine/cli/README.md`](../cli/README.md); this file explains how the inside fits together.

Native on macOS, Windows, and Linux, including headless servers. It never uses the OS keychain or the desktop app's data dir.

## Where it sits

```
 Claude Code / Codex / any MCP client          you, in a terminal
            │ stdio (MCP)                             │
            ▼                                         ▼
     dashbeam mcp ─────────┐              dashbeam send / pair / inbox …
                           │  newline-delimited JSON over <data dir>/node.sock
                           │  (Unix socket 0600, or a local Windows named pipe)
                           ▼
                ┌──────────────────────────┐
                │  dashbeam up (daemon)    │  engine/cli: ipc.rs, daemon.rs, commands.rs
                │  one per data dir        │  NodeLock = node.lock
                └────────────┬─────────────┘
                             │ Rust calls
                ┌────────────▼─────────────┐
                │  AgentService            │  this crate
                │  registry · inbound ·    │
                │  outbound · tickets ·    │
                │  pairing · cancels       │
                └────────────┬─────────────┘
                             │ NodeService API + EventEmitter events
                ┌────────────▼─────────────┐
                │  engine/native NodeService│  pairing, presence, Nearby (mDNS),
                │  + engine send/receive    │  invites, blob shares (iroh / QUIC)
                └──────────────────────────┘
```

The desktop app and the agent are separate nodes with separate identities, even on the same machine. The app shows the agent as a paired device whose `device_type` is `"agent"` (bot icon, "Agent" label).

## Modules

| File | What it owns |
|---|---|
| `service.rs` | `AgentService::start` / `shutdown`, the agent identity options, the event loop that routes engine events, `cancel_transfer`. |
| `paths.rs` | Data dir resolution (`--data-dir` > `$DASHBEAM_AGENT_DIR` > `<OS data dir>/dashbeam-agent`), the guard that refuses the app's `com.dashbeam.desktop` dir, `node.sock` / `node.lock` / `node.log` paths. |
| `config.rs` | `config.toml` (`AgentConfig`, `[[pairing.authorized]]`). Unknown keys are rejected, so the dangerous flag can never be set from config. |
| `lock.rs` | `NodeLock`: an exclusive file lock, so only one node runs per data dir. |
| `events.rs` | `ChannelEmitter`: turns engine events into a channel the event loop reads. |
| `approver.rs` | `Approver` trait (the human-in-the-loop hook) and `DenyAll` for non-interactive nodes. The CLI's `TtyApprover` implements it for an interactive `dashbeam up`. |
| `registry.rs` | In-memory `TransferRecord`s (cap 500, oldest finished ones evicted first) with a `seq` counter that `wait_*` calls watch. |
| `cancel.rs` | Cancel slots shared by sends and downloads (`Pending` → `Running` → removed). |
| `devices.rs` | `resolve_device`: exact endpoint id, then case-insensitive name, then a unique id prefix of 4+ chars. It never guesses between matches. |
| `metadata.rs` | `prepare_paths`: checks every path before sharing (exists, is a regular file or folder), counts files and bytes. |
| `outbound.rs` | Sends to paired devices: one worker ("lane") per device, invite, wait for answer, finish. |
| `inbound.rs` | Invites arriving from other devices: auto-accept, hold for an answer, or decline; downloads; nearby pair requests. |
| `tickets.rs` | `share_ticket` (one-time, expiring blob ticket) and `receive_ticket`. |
| `pairing.rs` | Pairing code / host / join, Nearby pair requests, trust, forget, `pairing.authorized` auto-pairing at startup, and the flag-gated `agent_*` variants. |

## Lifecycle

`AgentService::start(AgentOptions { data_dir, config, allow_agent_pairing, discoverable_override, approver })`:

1. Removes `IROH_SECRET` from the environment so neither the node nor its share sessions can use it.
2. Builds relay and discovery settings from config. `discoverable_override` (`up --discoverable`) forces Nearby visibility to everyone.
3. Starts `NodeService::start_with(..., agent_identity_options())`. Identity is file-based in the data dir, no keychain, `device_type = "agent"`, and the default name is `"<host name> Agent"`.
4. Applies `name` from config and hands `pairing.authorized` ids to the node (`set_authorized_peers`).
5. Spawns the event loop, the auto-accept worker, and one join task per authorized peer (exponential backoff, 5 s up to 5 min).

`shutdown()` is idempotent. It signals every task to stop, waits for the event loop, aborts the workers, then shuts the node down.

The CLI is what takes the `NodeLock`, opens the socket, and picks the approver (`TtyApprover` when `up` runs in a terminal, `DenyAll` when it was auto-started in the background).

## Trust model in code

| Sender's state on this node | What happens to its invite |
|---|---|
| Paired, active, **trusted** | Auto-accepted, downloaded into `<receive_dir>/<sender>/`. |
| Paired, active, not trusted | Held as a pending invite until `accept_incoming`, `decline_incoming`, the approver (interactive terminal only), or expiry (`invite_timeout_secs`, then a decline is sent). |
| Not paired (or a pairing no longer active), but in `pairing.authorized` | Accepted without asking, which pairs it. |
| Not paired, not authorized | Declined, unless an interactive terminal says yes. With `--dangerously-allow-agent-pairing` it is held so an agent can `accept_incoming` it, which pairs that device. |

Incoming **pair requests** (Nearby) are accepted only for `pairing.authorized` ids or a human's `y`. The dangerous flag never changes that. All the flag does is add the `agent_pair_nearby` / `agent_trust` calls and let agents accept held nearby invites.

`trust = true` on an authorized peer is applied once, when that peer newly pairs. A manual `untrust` sticks; forgetting and re-pairing counts as new again.

The MCP tool list in `engine/cli/src/mcp.rs` is the security boundary for agents: without the flag, no tool can pair, trust or forget. A CLI test pins the exact tool set.

## Transfers

Every transfer is one `TransferRecord` in the registry. It gets **exactly one terminal update**, written by whoever owns it at that moment; progress updates go through `update_live`, which does nothing once a record is terminal. Serialized records carry `transfer_id` (same as `id`) and an RFC 3339 `expires_at` alongside `expires_at_ms`.

Statuses: `queued`, `starting`, `waiting_for_accept`, `waiting_for_download`, `transferring` (live) and `delivered`, `received`, `declined`, `expired`, `unreachable`, `failed`, `cancelled` (terminal).

### Send to a paired device (`send_to_device`)

```
queued ─▶ starting ─▶ waiting_for_accept ─▶ transferring ─▶ delivered
  (busy)      │         (untrusted on       (accepted or    │
              │          our side)          bytes moving)   ├▶ declined
              ├▶ unreachable (invite not delivered)         ├▶ expired  (no answer in invite_timeout_secs)
              └▶ failed                                     ├▶ failed   (accepted, then no data for that long)
                                                            └▶ cancelled
```

- The call returns at once with the record; `wait_for_transfer` (or `send --wait`) gives the outcome.
- **One outstanding invite per device.** The wire's invite response has no invite id, so the agent can only tell answers apart by device. Each device gets a lane that runs its sends in order; different devices run in parallel. After an `expired` send, the lane waits 5 s before the next one so a late answer is not taken as the next send's answer.
- If the receiver finishes downloading, the send is `delivered` no matter what else ended the wait (cancel, shutdown, a late decline).

### Receive from a paired device

Invites arrive as the engine's `paired-invite-received` event and are routed by the table above. Trusted invites go through one auto-accept worker (queue cap 20, duplicates dropped). Pending invites keep one FIFO per sender; `list_incoming` shows the oldest per device.

Downloads into the same folder run one at a time, because the engine's "does the file exist? then copy" step could otherwise overwrite. Existing files are never overwritten; conflicts get renamed, and `saved_paths` lists the real per-file paths. Sender names are sanitized into one safe folder name. Received files are never opened or executed.

### Tickets (`share_ticket`, `receive_ticket`)

For agents outside your paired network. `share_ticket` returns a sendme-compatible blob ticket at once (`waiting_for_download`) and stops sharing after the first completed download (`delivered`) or at `ticket_ttl_secs` (`expired`). A download still moving bytes at expiry gets 30 s grace windows, never more than 10 minutes past expiry. One-time is best effort: a second downloader already mid-transfer may also finish.

`receive_ticket` downloads into `<receive_dir>/tickets/<transfer id>/` and returns as soon as the download starts. With `max_ticket_receive_bytes` set, it reads the sender's metadata first and refuses an oversized payload before downloading.

### Cancel

`cancel_transfer` works on queued and running sends, ticket shares and downloads. A transfer that never started is finalized by the canceller; a running one is signalled and finalizes itself (the call waits up to 10 s). Cancelling an already finished transfer returns it unchanged.

## Engine changes this crate relies on

Made in `engine/native` for the agent; the desktop app keeps its old behavior through the default options.

- `IdentityOptions { use_keychain, honor_env_secret, device_type, default_display_name }` with `load_or_create_identity_with` and `NodeService::start_with`.
- `NodeService::set_authorized_peers`: authorized peers can pair by code or Nearby request without an open pairing host.
- The facade (`engine/src/lib.rs`) re-exports `default_display_name`.

## Tests

```bash
CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only \
  cargo test --manifest-path engine/Cargo.toml -p dashbeam-agent -p dashbeam-cli -- --test-threads=1
```

The env vars keep `engine/target` from growing huge. Tests bind real endpoints, so run them single-threaded and never alongside the `src-tauri` tests.

| File | Covers |
|---|---|
| unit tests in `src/` | paths and app-dir guard, config, lock, registry, cancel slots, device resolution, routing tables, path checks |
| `tests/e2e_core.rs` | agent identity type, separate data dir, config name, rename |
| `tests/e2e_pairing.rs` | authorized peers pair at startup, `trust = true` once, manual untrust sticks, forget then re-pair, flag-gated wrappers |
| `tests/e2e_send.rs` | trusted delivery, decline, expiry, late decline after expiry, per-device queueing, unknown device, cancel |
| `tests/e2e_inbound.rs` | trusted auto-receive, pending accept / decline / expiry, hostile sender names, nearby invites with and without the flag, pre-approved nearby |
| `tests/e2e_tickets.rs` | one-time ticket, expiry, cancel, size cap, sendme compatibility |

The agent tests never touch your OS keychain. Nearby discovery is injected rather than using real mDNS, so a two-machine LAN check is still a manual step.

## Known limits

- One outstanding invite per device (no invite id on the wire); later sends queue.
- One-time tickets are best effort under concurrent downloaders.
- `max_ticket_receive_bytes` trusts the size in the sender's metadata; plain `sendme` tickets carry none and are rejected when the limit is set.
- The registry is in memory: transfer history is gone after the node restarts.
- `pairing.authorized` is read only at startup.
- Rare cases: a trusted receive can wait behind a same-sender folder lock, and an authorized peer's Nearby message can be dropped while a human pairing host is open.

## Roadmap

This is phase 1 (reliable device handoff). Next: agent inbox and job manifests, completion notifications, permissions and audit logs, one-command install with MCP examples.
