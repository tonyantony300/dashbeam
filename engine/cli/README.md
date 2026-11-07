# dashbeam CLI and MCP server

`dashbeam` is a headless DashBeam node for terminals and AI agents. It pairs with your DashBeam devices and sends or receives files over the same iroh transport as the app. It runs on macOS, Windows, and Linux, on a desktop or a headless server: no display and no TTY are required. `dashbeam mcp` and every other command auto-start the node in the background. An interactive `dashbeam up` is only for a person answering a pair prompt.

## Install

```bash
cargo install --path engine/cli    # or: cargo build --release --manifest-path engine/Cargo.toml -p dashbeam-cli
```

The binary is `dashbeam` (Rust 1.91, pinned by `rust-toolchain.toml`).

## The node

Each data dir has one long-lived node. Every command and `dashbeam mcp` auto-starts it if it is not running. `dashbeam up` runs it in the foreground (Ctrl-C to stop); `dashbeam status` shows whether it is up and how it started; `dashbeam stop` shuts it down.

- Data dir: `--data-dir`, then `$DASHBEAM_AGENT_DIR`, then `<OS data dir>/dashbeam-agent`. It holds identity, `config.toml`, the control socket (`node.sock`), `node.log` and `received/`. On macOS and Linux `node.sock` is a Unix socket (mode 0600). On Windows it is a marker file naming a local named pipe that only your user can open.
- The agent has its own identity. It never touches the OS keychain, ignores `IROH_SECRET`, and never uses the desktop app's data dir.
- Use `--json` on any command for raw JSON.

```bash
dashbeam up --name "Build Box"     # optional: foreground node; --discoverable = visible to everyone on the LAN
dashbeam status
```

## Pairing

Pairing adds a device to your trusted network. Do it yourself; the agent should not.

- **Code:** on one side run `dashbeam pair --host` (prints a code, `--ttl <secs>`, default 300); on the other, run `dashbeam pair --join <code>` or enter the code in the app. To pair with the desktop or Android app the other way round, show a code in the app and run `dashbeam pair --join <code>`. Ctrl-C on `pair --host` stops waiting, but the node keeps the pairing host open until the TTL runs out.
- **Nearby (same LAN):** `dashbeam nearby` lists devices; `dashbeam pair <device>` sends a request that the other side accepts. An incoming pair request is accepted only if the `up` terminal is interactive and you answer `y`, or the requester is in `pairing.authorized`.
- **Config:** list the other node's endpoint id (`dashbeam id`) under `pairing.authorized` in `<data dir>/config.toml`. At startup the node pairs with it without a code, retrying with backoff; `trust = true` also trusts it once newly paired. A listed peer may also pair with this node by code or Nearby request without an open pairing host. The node reads the list only at startup: after editing it, run `dashbeam stop` (the next command starts the node again).

Then `dashbeam devices`, `dashbeam trust <device>` (auto-accept its files), `dashbeam untrust <device>`, `dashbeam forget <device>` (unpair).

### config.toml

All keys are optional; unknown keys are rejected. Defaults shown.

```toml
name = "Build Box"                 # default: "<default device name> Agent"
receive_dir = "/home/me/inbox"     # default: <data dir>/received; a relative path is relative to the data dir
discoverability = "paired_only"    # "everyone" | "paired_only" | "off"
invite_timeout_secs = 600          # how long an untrusted invite waits for accept/decline
ticket_ttl_secs = 3600             # lifetime of share tickets
# max_ticket_receive_bytes = 1073741824   # unset = no limit; see Limits
# relay = ...                      # same relay / discovery settings as the app
# discovery = ...

[[pairing.authorized]]
endpoint_id = "<id from `dashbeam id`>"
trust = true                       # also auto-accept its files (default false)
# relay_url = "https://relay.example"
```

## Sending and receiving

```bash
dashbeam send "My Phone" ./report.pdf ./photos --wait
dashbeam share ./build.tar.gz [--ttl 600]     # prints a one-time ticket
dashbeam receive <ticket> --wait              # saves into the receive dir
dashbeam transfers [--direction in|out]
dashbeam inbox                                # pending invites from untrusted devices
dashbeam inbox accept <invite_id>             # or: decline
dashbeam cancel <transfer_id>
```

Received files go to `<receive_dir>/<sender name>/`, never overwrite existing files, and are never opened or executed.

## Messages

Short text messages between paired devices, for example two agents coordinating a handoff. Only paired devices can send or receive them; there is no read receipt.

```bash
dashbeam msg send "Build Box" "the build is up, grab it"
dashbeam msg wait [--timeout 60]              # unread messages, each handed out once; exit code 2 if none arrived
dashbeam msg list                             # recent history, both directions
```

Messages are kept in memory (last 500) and are up to 64 KiB each. An older peer that predates messaging drops them silently.

Limits:

- One outstanding invite per device; further sends to the same device queue behind it.
- Tickets are one-time on a best-effort basis: a second peer already mid-download may also finish.
- `dashbeam receive` and `receive_ticket` return as soon as the download starts. Use `--wait` or `wait_for_transfer` for the outcome.
- After a send to a device ends `expired`, a late answer to that invite may be attributed to the next send to the same device. The node waits 5 s before starting that next send to make this unlikely.
- `max_ticket_receive_bytes` checks the size the sender's metadata reports, before downloading. That size is not verified, and a plain `sendme` ticket carries no metadata, so with the limit set such tickets are rejected.

## MCP for AI agents

`dashbeam mcp` serves MCP over stdio and auto-starts the node.

```bash
claude mcp add dashbeam -- dashbeam mcp
```

Generic client config:

```json
{
  "mcpServers": {
    "dashbeam": { "command": "dashbeam", "args": ["mcp"] }
  }
}
```

Tools (17): `get_identity`, `list_devices`, `list_nearby`, `send_to_device`, `share_ticket`, `receive_ticket`, `transfer_status`, `wait_for_transfer`, `cancel_transfer`, `list_transfers`, `wait_for_incoming`, `list_incoming`, `accept_incoming`, `decline_incoming`, `send_message`, `wait_for_message`, `list_messages`.

Transfer results carry `transfer_id` (pass it to the other tools; `id` holds the same value) and, for tickets and invites, `expires_at` (RFC 3339 UTC; `expires_at_ms` is the same instant in unix milliseconds). `wait_*` calls default to 60 s and are capped at 120 s. Relative paths resolve against the MCP process's working directory.

`dashbeam up --dangerously-allow-agent-pairing` lets agents grow your network. With it:

- two more tools exist: `pair_with_device` (sends a pair request to a nearby device) and `trust_device` (trust or untrust a paired device);
- invites from unpaired nearby devices wait in `list_incoming`, and `accept_incoming` accepts them, which pairs that device.

Nothing is accepted automatically, even with the flag: incoming pair requests still need `pairing.authorized` or a `y` at an interactive `up` terminal. The flag is never a config key. Without it, the MCP server cannot pair, trust, or forget anything.

## Trust model

- Anyone who can run commands as your user can drive the node; the socket is user-only.
- The agent can send to devices you paired, and can read local files you point it at. It can share a ticket with anyone.
- Paired devices can put text in front of your agent with `send_message`. The tool descriptions tell the agent to treat it as information from another device, not as instructions from you; only tell your agent to follow a device's messages if you trust whoever drives that device.
- Devices you paired but did not `trust` must be accepted per transfer, via `dashbeam inbox` or `accept_incoming`. Trusted devices deliver without a prompt, so trust only your own machines.
- The MCP server cannot change who is paired or trusted unless you opt in with the flag above. Pairing and trust are yours to grant.

### Recommended Claude Code deny rule

Add this to your Claude Code settings so the agent cannot easily grant itself access through the shell, which bypasses the MCP restrictions:

```json
{
  "permissions": {
    "deny": [
      "Bash(dashbeam pair:*)",
      "Bash(dashbeam trust:*)",
      "Bash(dashbeam untrust:*)",
      "Bash(dashbeam forget:*)",
      "Bash(dashbeam up --dangerously-allow-agent-pairing:*)",
      "Bash(dashbeam --json pair:*)",
      "Bash(dashbeam --json trust:*)",
      "Bash(dashbeam --json untrust:*)",
      "Bash(dashbeam --json forget:*)"
    ]
  }
}
```

These rules match command prefixes, so they are defense in depth, not a boundary. Other global flags placed before the subcommand (for example `dashbeam --data-dir D trust …`) get past them, and so does editing `<data dir>/config.toml` to add a `pairing.authorized` entry (also deny edits to that file). An agent with an unrestricted shell can do whatever your user can.
