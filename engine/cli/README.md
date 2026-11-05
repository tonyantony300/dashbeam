# dashbeam CLI and MCP server

`dashbeam` is a headless DashBeam node for terminals and AI agents. It pairs with your DashBeam devices and sends or receives files over the same iroh transport as the app. macOS and Linux only.

## Install

```bash
cargo install --path engine/cli    # or: cargo build --release --manifest-path engine/Cargo.toml -p dashbeam-cli
```

The binary is `dashbeam` (Rust 1.91, pinned by `rust-toolchain.toml`).

## The node

Each data dir has one long-lived node. Every command and `dashbeam mcp` auto-starts it if it is not running. `dashbeam up` runs it in the foreground (Ctrl-C to stop); `dashbeam status` shows whether it is up and how it started; `dashbeam stop` shuts it down.

- Data dir: `--data-dir`, then `$DASHBEAM_AGENT_DIR`, then `<OS data dir>/dashbeam-agent`. It holds identity, `config.toml`, the control socket (`node.sock`, mode 0600), `node.log` and `received/`.
- The agent has its own identity. It never touches the OS keychain, ignores `IROH_SECRET`, and never uses the desktop app's data dir.
- Use `--json` on any command for raw JSON.

```bash
dashbeam up --name "Build Box"     # optional: foreground node; --discoverable = visible to everyone on the LAN
dashbeam status
```

## Pairing

Pairing adds a device to your trusted network. Do it yourself; the agent should not.

- **Code:** on one side run `dashbeam pair --host` (prints a code, `--ttl <secs>`, default 300); on the other, run `dashbeam pair --join <code>` or enter the code in the app. The agent can also join a code from the app.
- **Nearby (same LAN):** `dashbeam nearby` lists devices; `dashbeam pair <device>` sends a request that the other side accepts.
- **Config:** list the other node's endpoint id (`dashbeam id`) under `pairing.authorized` in `<data dir>/config.toml`. At startup the node pairs with it without a code, retrying with backoff; `trust = true` also trusts it once newly paired.

Then `dashbeam devices`, `dashbeam trust <device>` (auto-accept its files), `dashbeam untrust <device>`, `dashbeam forget <device>` (unpair).

### config.toml

All keys are optional; unknown keys are rejected. Defaults shown.

```toml
name = "Build Box"                 # default: "<default device name> Agent"
receive_dir = "/home/me/inbox"     # default: <data dir>/received
discoverability = "paired_only"    # "everyone" | "paired_only" | "off"
invite_timeout_secs = 600          # how long an untrusted invite waits for accept/decline
ticket_ttl_secs = 3600             # lifetime of share tickets
# max_ticket_receive_bytes = 1073741824   # unset = no limit
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

Limits:

- One outstanding invite per device; further sends to the same device queue behind it.
- Tickets are one-time on a best-effort basis: a second peer already mid-download may also finish.
- `receive_ticket` (MCP) blocks until the download finishes.

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

Tools (14): `get_identity`, `list_devices`, `list_nearby`, `send_to_device`, `share_ticket`, `receive_ticket`, `transfer_status`, `wait_for_transfer`, `cancel_transfer`, `list_transfers`, `wait_for_incoming`, `list_incoming`, `accept_incoming`, `decline_incoming`.

Transfer results use `id` for the transfer id (pass it as `transfer_id` to the other tools) and `expires_at_ms` for ticket expiry. `wait_*` calls default to 60 s and are capped at 120 s. Relative paths resolve against the MCP process's working directory.

Two more tools, `pair_with_device` and `trust_device`, exist only if the node was started with `dashbeam up --dangerously-allow-agent-pairing`. That flag is never a config key. Without it, the MCP server cannot pair, trust, or forget anything.

## Trust model

- Anyone who can run commands as your user can drive the node; the socket is user-only.
- The agent can send to devices you paired, and can read local files you point it at. It can share a ticket with anyone.
- Devices you paired but did not `trust` must be accepted per transfer, via `dashbeam inbox` or `accept_incoming`. Trusted devices deliver without a prompt, so trust only your own machines.
- The MCP server cannot change who is paired or trusted unless you opt in with the flag above. Pairing and trust are yours to grant.

### Recommended Claude Code deny rule

Add this to your Claude Code settings so the agent cannot grant itself access through the shell, which bypasses the MCP restrictions:

```json
{
  "permissions": {
    "deny": [
      "Bash(dashbeam pair:*)",
      "Bash(dashbeam trust:*)",
      "Bash(dashbeam untrust:*)",
      "Bash(dashbeam forget:*)",
      "Bash(dashbeam up --dangerously-allow-agent-pairing:*)"
    ]
  }
}
```
