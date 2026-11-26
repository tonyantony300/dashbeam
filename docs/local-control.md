# Linux local control (experimental)

DashBeam can optionally expose its running transfer engine to desktop shells and
other programs running as the same user. For example, a DankMaterialShell widget
can list paired/nearby devices and send files without showing the main window.
The app still runs in the background; this is not a standalone transfer daemon.

This interface is experimental. The wire format and interaction with the main
window's transfer/invitation dialogs are subject to review.

## Enable

Start a Linux desktop build with:

```sh
DASHBEAM_LOCAL_CONTROL=1 DashBeam --hidden
```

Quit an existing DashBeam instance first: launching a second copy only forwards
its arguments to the existing instance, so its environment cannot enable control.
Use the executable's actual path if your package does not provide `DashBeam`.
Normal launches, Windows, macOS, Android, and the web build are unchanged.
A socket setup failure is logged but does not prevent the desktop app from starting.

## Access model

- Disabled unless `DASHBEAM_LOCAL_CONTROL` is exactly `1`.
- Socket: `$XDG_RUNTIME_DIR/dashbeam/control.sock` (0600), directory 0700.
- Runtime/control directories must be real, private directories owned by the
  effective user. Symlinks, foreign paths, and non-socket files are refused.
- A live listener is never replaced. A stale owned socket is recovered.
- Linux peer credentials must match the application's effective UID.
- No TCP/HTTP listener, shell commands, arbitrary command dispatch, or new
  transfer-network protocol. Existing DashBeam commands perform transfers.
- Same-user clients can read device information and tickets, share readable
  files, write received files, and pair devices. Treat access as full control of
  this user's DashBeam instance. This is not a multi-user authorization layer.
- Requests are limited to 64 KiB, newline terminated, with a five-second read
  timeout and at most eight simultaneous connections. Unknown fields/actions
  and invalid field types are rejected.

## Protocol version 1

Open an AF_UNIX stream, write one UTF-8 JSON object followed by `\n`, and read one
JSON response followed by `\n`. The connection closes after that response.
Success: `{"ok":true,"data":...}`. Failure: `{"ok":false,"error":"..."}`.
Do not log tickets or device snapshots in a shared/public log.

| Action | Fields | Result |
| --- | --- | --- |
| `status` | none | `api_version`, `node`, `paired`, `nearby`, `ticket`, `active`, `events` |
| `send` | `paths` (absolute paths), optional `endpoint_id`, `nearby` | Share ticket; targeted sends also report `delivered` and optionally `invite_error` |
| `receive` | `ticket`, `output` (absolute directory) | Starts a receive; completion/errors arrive in later status snapshots |
| `stop` | none | Cancels receiving and stops the current share |
| `respond` | `endpoint_id`, `accepted`, `output` if accepting | Responds to the matching pending invite and starts receiving when accepted |
| `pairing` | none | Opens a pairing host for 120 seconds and returns its code |
| `join` | `ticket` | Joins a pairing host |
| `pair_nearby` | `endpoint_id` | Requests pairing with a nearby device; reports `delivered` |

`status.node` distinguishes startup/unavailable state from an empty device list.
`paired` and `nearby` use DashBeam's existing device records. Offline paired
devices remain listed; nearby discovery depends on discoverability and multicast.

`events` contains the latest payload for each observed transfer/invitation event,
not a durable event queue. Nested JSON payloads are decoded; transfer telemetry
keeps the existing `bytes:total:speed_x1000` form. Transfer progress/completion
snapshots reset when a client starts a transfer. Only one explicit receive runs
through the control interface at a time; the engine also enforces its own limits.
A targeted send keeps its ticket available if delivery fails, allowing the user
to share the ticket instead.

For incoming invitations, `respond` uses the ticket recorded by the running app,
not a ticket supplied by the caller. Sender matching and output validation happen
before the response is sent. Paired/trusted auto-accept preferences are unchanged.

## DankMaterialShell example

See [the example widget](../examples/dank-material-shell/README.md) for a QML bar
widget, Python socket client, optional systemd unit, and isolated end-to-end test.
These are examples: no installation script modifies the user's shell or data.

## Tests

```sh
cargo test --manifest-path src-tauri/Cargo.toml --lib local_control::tests
python3 -m unittest discover -s examples/dank-material-shell/tests -p 'test_*.py'
python3 examples/dank-material-shell/tests/e2e.py \
  --binary /absolute/path/to/the/built/DashBeam
```

The end-to-end test starts two isolated desktop instances with synthetic data,
pairs them, verifies online presence, and compares received bytes for both a
ticket transfer and an accepted device invitation. It needs a working Wayland
session and network connectivity for node initialization. It does not use the
user's device identity or send files to the user's paired devices.
