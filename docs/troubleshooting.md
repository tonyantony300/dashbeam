# Troubleshooting

Find the symptom that matches what you're seeing. Each section lists the most common
causes first.

- [The app won't open](#the-app-wont-open)
- [The window is blank or white](#the-window-is-blank-or-white)
- [I can't connect to the other device](#i-cant-connect-to-the-other-device)
- [A transfer stalls or fails partway](#a-transfer-stalls-or-fails-partway)
- [A paired device shows as Offline](#a-paired-device-shows-as-offline)
- [The Nearby list is empty](#the-nearby-list-is-empty)
- [I can't find the files I received](#i-cant-find-the-files-i-received)
- [Updates don't install](#updates-dont-install)
- [Collecting logs](#collecting-logs)
- [Reporting a bug](#reporting-a-bug)

Still stuck? Open an [issue](https://github.com/tonyantony300/dashbeam/issues/new/choose)
or ask in [Discord](https://discord.gg/xwb7z22Eve).

---

## The app won't open

Nothing appears when you launch it, or it closes immediately.

**It may already be running.** DashBeam allows only one instance. If it's minimised to
the system tray, launching again just focuses the existing window instead of opening a
new one. Check your tray or menu bar first.


**Windows.** Unsigned builds trigger SmartScreen on first run. Choose
**More info → Run anyway**, or verify your download against the checksums on the
[releases page](https://github.com/tonyantony300/dashbeam/releases) first.

**Windows: nothing happens at all.** DashBeam draws its interface with the Microsoft
Edge WebView2 Runtime, which isn't on every machine. If it's missing or damaged, the
app opens no window and no process shows in Task Manager. Install it from
[Microsoft](https://developer.microsoft.com/microsoft-edge/webview2) and launch again.

**macOS.** Builds are signed and notarised, so Gatekeeper shouldn't object. If macOS
says the app is damaged or from an unidentified developer, that's a bug worth reporting;
mention where you downloaded it from.

**Linux.** Make sure the AppImage is executable:

```bash
chmod +x DashBeam_*.AppImage
./DashBeam_*.AppImage
```

If it still won't start, [collect logs](#collecting-logs). A missing system library
usually shows up there immediately.

---

## The window is blank or white

The window opens at the right size but the contents are solid white or grey, and the
terminal shows no errors.

This is almost always **Linux-specific**. DashBeam's interface runs inside WebKitGTK
there, and a blank window means the web view couldn't initialise its GPU surface. The
app itself is running fine. It's usually a mismatch between the graphics libraries
bundled in the AppImage and the drivers on your system, so it's most common on
distributions shipping very recent Mesa versions (Arch, CachyOS, EndeavourOS,
openSUSE Tumbleweed).

**Try the Flatpak build first.** It uses a WebKitGTK matched to its runtime rather than
the copy bundled in the AppImage, which avoids this problem entirely. App ID is
`net.dashbeam.DashBeam`. Check the
[releases page](https://github.com/tonyantony300/dashbeam/releases) for availability.

**Or work through these in order**, and note which one renders correctly:

```bash
# 1. Disable GPU compositing in the web view
WEBKIT_DISABLE_COMPOSITING_MODE=1 ./DashBeam_*.AppImage

# 2. Disable the DMA-BUF renderer as well
WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 ./DashBeam_*.AppImage

# 3. Force X11 (only relevant if you're on Wayland)
GDK_BACKEND=x11 ./DashBeam_*.AppImage
```

If your machine has both integrated and discrete graphics (most desktop Ryzen and Intel
CPUs include an integrated GPU even with a graphics card fitted), also try pinning the
discrete card:

```bash
DRI_PRIME=1 ./DashBeam_*.AppImage
```

Knowing **which** of these works tells us exactly what to fix, so please include that in
your report even if you've found a workaround you're happy with. Useful extra detail:

```bash
echo "$XDG_SESSION_TYPE"   # wayland or x11
fastfetch --pipe           # or: inxi -GSx
ls -l /dev/dri/by-path/    # shows how many GPUs are present
```

Also say **how** you launched it: the `.AppImage` directly, or via Gear Lever /
AppImageLauncher / an extracted copy. Some graphics workarounds only activate when the
AppImage runs unextracted, so this changes the diagnosis.

If you see a blank window on **macOS or Windows**, that's a different and more unusual
problem. Please report it with logs.

---

## I can't connect to the other device

The ticket was shared but the transfer never starts, or the receiver sits waiting.

**The sender must keep the app open.** Files are served directly from the sending
device, and there's no copy on a server. If the sender closes DashBeam, sleeps their
machine, or loses network, the transfer can't proceed.

**Both sides need the same relay configuration.** Open **Settings → Network → Relay
servers** on both devices and press **Test connection**. Things to check:

- If the mode is **Disabled**, connections only work on your local network and will fail
  across the internet.
- If either side uses **Custom self-hosted** relays, both sides generally need the same
  relay URLs configured. A mismatch shows up as a connection that never establishes.
- If a custom relay requires an auth token, it must match `access.shared_token` on the
  server.

**Check the ticket was copied completely.** Tickets are long; a truncated paste fails to
parse or points nowhere.

**Restrictive networks.** Corporate, university, and guest Wi-Fi networks often block the
peer-to-peer traffic DashBeam relies on. Testing both devices on a different network is
the quickest way to confirm this is the cause.

---

## A transfer stalls or fails partway

**Speed depends on both devices' connections**, and on whether a direct connection could
be established. When NAT traversal fails, traffic falls back to an encrypted relay, which
is noticeably slower. **Settings → Network → Relay servers → Test connection** shows
whether your relay is reachable and its latency.

**The web version is always relayed** and has limited throughput by design. For large
transfers, use the desktop app on both ends.

**Don't let either machine sleep.** Suspending the sender or receiver mid-transfer
interrupts it.

**Check free disk space** on the receiving device, and that the destination folder is
still writable. DashBeam reports `Selected download folder is not writable` if
permissions changed after you chose it.

---

## A paired device shows as Offline

Paired devices appear under **Settings → Devices** with a status of Online, Offline,
Connecting, Unpaired, or Re-pair required.

- **Offline.** The other device isn't running DashBeam, or can't currently be reached.
  It stays paired; you can send a one-time ticket instead in the meantime.
- **Re-pair required.** The pairing is no longer valid and needs to be set up again via
  **Pair new device**.
- **Offline on both sides despite both being open.** Usually a relay or network mismatch.
  Work through [I can't connect to the other device](#i-cant-connect-to-the-other-device).

Changing relay settings can leave paired devices unreachable until *both* sides can reach
each other on the new configuration. The devices stay listed either way.

Pairing codes expire after a short window. If a code stops working, generate a fresh one
with **Show pairing code**.

On desktop, presence needs DashBeam to be running. If you close the window with
**Keep running in the background** enabled, the tray / menu bar icon keeps the process
alive. You can also enable **Start at login** under **Settings → General → Startup &
background** so paired devices can see you online without opening the window first.

---

## The Nearby list is empty

The **Nearby** section in **Settings → Devices** (and under **Send to a device** while
sharing) shows either "No devices found nearby" / "No devices nearby" or
"Nearby is unavailable", even though you have other DashBeam devices on the same network.

**Your network blocks multicast or mDNS.** Local discovery depends on
[mDNS](https://en.wikipedia.org/wiki/Multicast_DNS) (also called Bonjour on macOS), which
works only when devices can broadcast to each other on the same subnet.

- **Tunnel-all VPNs hide the LAN.** If either device is connected to a VPN that routes
  *all* traffic through the tunnel, it can't see or be seen on the local network, even if
  both devices are on the same Wi-Fi. Disconnect the VPN, or configure it to split-tunnel
  local subnets.
- **Guest Wi-Fi often blocks peer-to-peer.** Many access points prevent devices on a guest
  network from seeing each other. Connect both devices to the main (non-guest) Wi-Fi instead.
- **Some corporate and managed networks block multicast.** Universities, offices, and
  managed Wi-Fi systems often filter mDNS packets as a matter of policy. Check with your
  network administrator, or try a different network to confirm that's the cause.

**If this is your network, you can still transfer files normally.** Use a manual ticket
instead—ticket transfers connect directly (or via relay) and don't need mDNS. Paired
devices (after an initial setup elsewhere on a working network) will also continue to
work, because they use a direct connection or relay and don't depend on multicast.

You can control whether your device appears to others on the local network via
**Settings → Network → Your discoverability** (Everyone / Paired only / Off). Changing
this setting won't solve the Nearby list being empty if multicast is blocked; it only
hides your device from those who *can* see the network.

---

## I can't find the files I received

**Check whether you picked a folder.** If no destination was chosen, DashBeam saves into
its own private application folder rather than your Downloads directory. The receive
screen shows **App downloads (default)** when this is the case, and you'll see a notice
naming the path after the transfer completes.

That private folder can't always be opened from a file manager. If you try, DashBeam
tells you the files are in its private folder and asks you to choose a download folder
instead. To avoid this, set a destination with **Save to folder** *before* accepting a
transfer.

**Flatpak** builds can only write to your Downloads directory by default. Choosing a
location elsewhere may fail unless you grant extra filesystem access with Flatseal.

---

## Updates don't install

**Package-managed installs don't self-update.** If you installed via Flatpak, `.deb`, or
`.rpm`, update through your package manager or by installing the new release. The
in-app updater doesn't manage those.

**Automatic checks can be turned off.** See **Settings → General → Automatically check
for updates**.

**Update checks need network access** to GitHub. If GitHub is blocked or unreachable on
your network, checks fail silently.

---

## Collecting logs

There are two ways to capture logs. **Debug mode** is easier and works the same on every
platform; the `RUST_LOG` commands below are the fallback for when the app won't start or
won't render.

### Debug mode (recommended)

Best for problems with transfers, connections, relays, discovery, and pairing.

1. Open **Settings → General** and turn on **Debug logging**.
2. **Restart DashBeam.** Logging is off until you do, because the switch takes effect at launch.
3. Reproduce the problem.
4. Return to **Settings → General** and choose **Save diagnostics…**.
5. Attach the saved `.txt` to your bug report.

The file contains a summary of your device, app version, OS, and relay configuration,
followed by the captured logs. **Open it and read it before sending.** It includes local
and remote IP addresses, device identifiers, relay URLs, your device name, and the names
of files you shared. Nothing is uploaded anywhere; the file only leaves your machine if
you attach it yourself.

Turn **Debug logging** off when you're done, which deletes the captured logs immediately.
They're also size-capped and pruned automatically while it's on, and the setting switches
itself off after 7 days if you forget.

> Debug mode can't help if DashBeam won't open or shows a blank window, since you can't
> reach Settings. Use the `RUST_LOG` commands below for those.

### RUST_LOG (fallback)

DashBeam writes logs to standard output. Setting `RUST_LOG=debug` raises the detail level
well above the default, and is often the difference between "no errors" and an actionable
report. This is the only option for launch and rendering failures.

> **Quit DashBeam completely first.** Only one instance runs at a time, so if a window is
> already open, launching again hands off to the existing instance and exits immediately,
> leaving you with an empty log. Quit from the tray or menu bar icon too, because closing
> the window may leave it running.

### Linux

```bash
RUST_LOG=debug ./DashBeam_*.AppImage 2>&1 | tee dashbeam.log
```

For display or rendering problems, add the graphics driver's own diagnostics. The blank
window failure happens inside WebKit's separate rendering process, which doesn't log
through DashBeam:

```bash
RUST_LOG=debug LIBGL_DEBUG=verbose EGL_LOG_LEVEL=debug MESA_DEBUG=1 \
  ./DashBeam_*.AppImage 2>&1 | tee dashbeam.log
```

### macOS

Launch the binary inside the app bundle. Double-clicking in Finder discards output.

```bash
RUST_LOG=debug /Applications/DashBeam.app/Contents/MacOS/DashBeam 2>&1 | tee dashbeam.log
```

### Windows

Release builds are GUI applications with no attached console, so nothing appears in the
terminal and you have to redirect to a file. Use `cmd.exe`, not PowerShell:

```cmd
set RUST_LOG=debug
"%LOCALAPPDATA%\DashBeam\DashBeam.exe" > "%USERPROFILE%\Desktop\dashbeam.log" 2>&1
```

For a system-wide install, the path is `"C:\Program Files\DashBeam\DashBeam.exe"`.

The prompt returns immediately while DashBeam keeps running, which is normal for GUI
applications. The file fills in as you use the app; close DashBeam before opening it.

---

## Reporting a bug

Include as much of this as you can:

- **DashBeam version**, and the exact file you installed (`.AppImage`, `.deb`, `.rpm`,
  Flatpak, `.msi`, `.exe`, `.dmg`, `.apk`)
- **OS and version.** On Linux, also your desktop environment and whether you're on
  Wayland or X11
- **Whether it ever worked**, and the last version it worked in
- **`dashbeam.log`** from the steps above
- **For transfer problems**, both devices' platforms, your relay mode, and whether
  **Test connection** succeeds
- **For display problems**, your graphics hardware

A screenshot helps, but on its own it rarely narrows anything down. The log is what makes
a report actionable.
