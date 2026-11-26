# DashBeam widget for DankMaterialShell

Experimental example for the opt-in [Linux local control API](../../docs/local-control.md).
It provides a bar indicator, paired devices with online/offline state, nearby
devices, file/folder sending, ticket receiving, incoming invitations, transfer
progress, cancellation, and pairing. The panel fits its contents and scrolls
only when it exceeds the available screen height.

## Requirements

- Linux desktop DashBeam built with the local control change, running with
  `DASHBEAM_LOCAL_CONTROL=1` (the stock release does not provide the socket).
- DankMaterialShell with the `PluginComponent` widget API and QtQuick.Dialogs.
- Python 3; `wl-copy` for the optional copy-ticket action.
- Both peers running DashBeam; discoverability enabled for nearby discovery.

## Manual installation

From the repository root:

```sh
mkdir -p ~/.config/DankMaterialShell/plugins/dashbeamDms ~/.local/bin
cp examples/dank-material-shell/{plugin.json,DashBeamWidget.qml} \
  ~/.config/DankMaterialShell/plugins/dashbeamDms/
install -m 700 examples/dank-material-shell/dashbeam-dms-client \
  ~/.local/bin/dashbeam-dms-client
```

Enable the plugin and add it to a bar using DMS settings. Quit an already-running
DashBeam, then launch the modified build:

```sh
DASHBEAM_LOCAL_CONTROL=1 /path/to/DashBeam --hidden
```

The example systemd unit is optional. Copy it to `~/.config/systemd/user/`, edit
`ExecStart` to your actual DashBeam path, and import the graphical-session
environment using your session manager before starting it. The widget's **Start
DashBeam in the background** button requires this unit. Coordinate it with any
existing DashBeam autostart entry so the unmodified instance does not start first.

```sh
systemctl --user daemon-reload
systemctl --user enable --now dashbeam-dms.service
```

No installer changes the existing app, updater, pairing data, startup settings,
or bar configuration. Keep backups before trying an experimental build.

## Usage

Select a paired online device or a nearby device, choose files/a folder, then
click **Send**. With **Shareable ticket**, copy the ticket for the recipient.
Paste a transfer ticket under **Receive**, choose the download directory, and
start receiving. Expand **Pair a device** to generate/join a pairing code or send
a pairing request to the selected nearby device.

The explicit download directory defaults to `~/Downloads` and is session-local.
Existing trusted-device auto-accept behavior continues to use DashBeam settings.
The widget polls every 15 seconds when closed and every 1.5 seconds when open.

Optional DMS IPC: `dms ipc call dashbeamDms toggle` toggles the panel;
`dms ipc call dashbeamDms status` reports connection and device counts.

## Limitations

This is an example client, not a replacement for all DashBeam settings. The
control API exposes a latest-event snapshot, not an invitation queue. The main
window may still have an invitation dialog for an invitation handled externally;
coordination between that UI and shell clients needs further design. Multiple
bars/screens can instantiate separate widget clients; the backend rejects
concurrent receives and keeps transfer state in the application.
