//! GNOME Files (Nautilus) "Send with DashBeam" entry.
//!
//! Nautilus lists every executable in `~/.local/share/nautilus/scripts` under
//! right-click → Scripts and hands it the selection in
//! `NAUTILUS_SCRIPT_SELECTED_FILE_PATHS`. This is the Linux counterpart of the
//! Windows Explorer verb in `platform::windows::context_menu`, with the same
//! ownership model: the running app writes it per user from the settings
//! toggle, and no package install script touches users' home directories.
//!
//! The script is marked with [`MARKER`] so a script the user wrote under the
//! same name is never overwritten or deleted. Because .deb/.rpm uninstalls and
//! deleted AppImages cannot reach per-user files, the script removes itself
//! the next time it runs once the DashBeam it points to is gone.

use anyhow::{bail, Context};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Menu label; matches the Windows Explorer verb.
const SCRIPT_NAME: &str = "Send with DashBeam";

/// Identifies a script DashBeam owns. Never change this string: older builds
/// recognise their own scripts by it.
const MARKER: &str = "# managed-by: dashbeam";

/// How the script starts DashBeam, decided by how this copy was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Launcher {
    /// A plain executable: .deb/.rpm, or an AppImage (`$APPIMAGE`).
    Executable(PathBuf),
    /// A Flatpak, started on the host with `flatpak run`.
    Flatpak(String),
}

/// True when Nautilus is likely this user's file manager, so the toggle is
/// worth offering. Deliberately permissive: a stray script on a system that
/// has Nautilus installed but rarely uses it costs nothing.
pub fn is_supported() -> bool {
    detect_nautilus(
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("XDG_CURRENT_DESKTOP").as_deref(),
    )
}

/// Write (or refresh) the script. A no-op where Nautilus is not detected, so
/// the default-on startup sync stays silent on other desktops.
pub fn register() -> anyhow::Result<()> {
    if !is_supported() {
        tracing::debug!("Nautilus not detected; skipping file manager script");
        return Ok(());
    }
    let dir = scripts_dir().context("could not resolve the home directory")?;
    let launcher = current_launcher()?;
    ensure_utf8(&launcher)?;
    write_script(&dir, &render_script(&launcher))
}

/// Remove the script if DashBeam wrote it. Runs even where Nautilus is no
/// longer detected, so turning the toggle off always cleans up.
pub fn unregister() -> anyhow::Result<()> {
    let Some(dir) = scripts_dir() else {
        return Ok(());
    };
    remove_script(&dir)
}

fn scripts_dir() -> Option<PathBuf> {
    // Under Flatpak `XDG_DATA_HOME` points into the sandbox; Flatpak passes
    // the host's value (when the host sets one) as `HOST_XDG_DATA_HOME`. The
    // manifest's `xdg-data/nautilus/scripts` grant follows the host value too.
    let var = if std::env::var_os("FLATPAK_ID").is_some() {
        "HOST_XDG_DATA_HOME"
    } else {
        "XDG_DATA_HOME"
    };
    data_home(
        std::env::var_os(var).as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .map(|data| data.join("nautilus").join("scripts"))
}

/// The host's `$XDG_DATA_HOME`, or `~/.local/share`. Relative values are
/// invalid per the XDG spec and ignored.
fn data_home(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(path) = xdg_data_home.map(PathBuf::from) {
        if path.is_absolute() {
            return Some(path);
        }
    }
    let home = PathBuf::from(home?);
    home.is_absolute()
        .then(|| home.join(".local").join("share"))
}

fn detect_nautilus(path: Option<&OsStr>, desktop: Option<&OsStr>) -> bool {
    // GNOME and its derivatives ship Nautilus: "GNOME", "ubuntu:GNOME",
    // "pop:GNOME", "zorin:GNOME", and Unity on older Ubuntu.
    let desktop_matches = desktop
        .and_then(OsStr::to_str)
        .map(|value| {
            value.split(':').any(|part| {
                part.eq_ignore_ascii_case("GNOME") || part.eq_ignore_ascii_case("Unity")
            })
        })
        .unwrap_or(false);
    if desktop_matches {
        return true;
    }
    // Nautilus installed elsewhere (e.g. on KDE). Not visible from a Flatpak,
    // where the desktop check above has to suffice.
    path.map(|path| std::env::split_paths(path).any(|dir| is_executable(&dir.join("nautilus"))))
        .unwrap_or(false)
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn current_launcher() -> anyhow::Result<Launcher> {
    if let Some(app_id) = std::env::var("FLATPAK_ID").ok().filter(|id| !id.is_empty()) {
        return Ok(Launcher::Flatpak(app_id));
    }
    // An AppImage runs from a temporary mount; `$APPIMAGE` is the file the
    // user actually has.
    if let Some(appimage) = std::env::var_os("APPIMAGE").map(PathBuf::from) {
        if appimage.is_absolute() {
            return Ok(Launcher::Executable(appimage));
        }
    }
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;
    Ok(Launcher::Executable(fs::canonicalize(&exe).unwrap_or(exe)))
}

/// The script embeds the path as text; refuse rather than write a script that
/// launches some other, lossily decoded path.
fn ensure_utf8(launcher: &Launcher) -> anyhow::Result<()> {
    match launcher {
        Launcher::Executable(path) if path.to_str().is_none() => {
            bail!("DashBeam's path is not valid UTF-8: {}", path.display())
        }
        _ => Ok(()),
    }
}

/// Single-quote for POSIX sh. Non-UTF-8 paths never get here (`ensure_utf8`).
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn render_script(launcher: &Launcher) -> String {
    let (installed_check, launch) = match launcher {
        Launcher::Executable(path) => {
            let quoted = shell_quote(&path.to_string_lossy());
            (
                format!("[ -x {quoted} ]"),
                format!("exec {quoted} \"$path\""),
            )
        }
        Launcher::Flatpak(app_id) => {
            let quoted = shell_quote(app_id);
            (
                format!(
                    "command -v flatpak >/dev/null 2>&1 && flatpak info {quoted} >/dev/null 2>&1"
                ),
                // --file-forwarding exports the path through the document
                // portal, so the sandboxed app can read it wherever it lives.
                format!("exec flatpak run --file-forwarding {quoted} @@ \"$path\" @@"),
            )
        }
    };

    format!(
        r#"#!/bin/sh
{MARKER}
# Written by DashBeam (Settings > General). Turn the option off there to
# remove it; edits are overwritten the next time DashBeam starts.

# DashBeam was uninstalled or moved: clean up instead of failing.
if ! {{ {installed_check}; }}; then
	rm -f -- "$0"
	exit 0
fi

# DashBeam opens one item per launch. Empty for remote locations (SMB, MTP),
# which DashBeam cannot read directly.
path=$(printf '%s\n' "$NAUTILUS_SCRIPT_SELECTED_FILE_PATHS" | head -n 1)
[ -n "$path" ] || exit 0

{launch}
"#
    )
}

fn is_owned_script(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|content| content.lines().nth(1) == Some(MARKER))
        .unwrap_or(false)
}

fn write_script(dir: &Path, content: &str) -> anyhow::Result<()> {
    let target = dir.join(SCRIPT_NAME);
    if target.symlink_metadata().is_ok() && !is_owned_script(&target) {
        bail!(
            "{} already exists and was not created by DashBeam; leaving it untouched",
            target.display()
        );
    }
    if fs::read_to_string(&target).ok().as_deref() == Some(content) {
        return Ok(());
    }

    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;

    // Write beside the target and rename, so Nautilus never lists a
    // half-written script.
    let tmp = dir.join(format!(".{SCRIPT_NAME}.{}.tmp", std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("could not write to {}", dir.display()))?;
        file.write_all(content.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(0o755))?;
        file.sync_all()?;
        fs::rename(&tmp, &target)
            .with_context(|| format!("could not create {}", target.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn remove_script(dir: &Path) -> anyhow::Result<()> {
    let target = dir.join(SCRIPT_NAME);
    if !is_owned_script(&target) {
        return Ok(());
    }
    fs::remove_file(&target).with_context(|| format!("could not remove {}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("dashbeam-nautilus-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn os(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }

    #[test]
    fn detects_gnome_based_desktops() {
        for desktop in ["GNOME", "ubuntu:GNOME", "pop:GNOME", "Unity", "gnome"] {
            assert!(detect_nautilus(None, os(desktop)), "{desktop}");
        }
    }

    #[test]
    fn ignores_other_desktops_without_nautilus() {
        let empty = TempDir::new("empty-path");
        let path = empty.0.to_str().unwrap();
        for desktop in [
            "KDE",
            "X-Cinnamon",
            "XFCE",
            "MATE",
            "niri",
            "GNOME-Flashback-ish",
        ] {
            assert!(!detect_nautilus(os(path), os(desktop)), "{desktop}");
        }
        assert!(!detect_nautilus(None, None));
    }

    #[test]
    fn detects_nautilus_on_path() {
        let bin = TempDir::new("bin");
        let nautilus = bin.0.join("nautilus");
        fs::write(&nautilus, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&nautilus, fs::Permissions::from_mode(0o644)).unwrap();
        let path = bin.0.to_str().unwrap();
        assert!(!detect_nautilus(os(path), os("KDE")), "not executable");
        fs::set_permissions(&nautilus, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(detect_nautilus(os(path), os("KDE")));
    }

    #[test]
    fn resolves_data_home() {
        assert_eq!(
            data_home(os("/data"), os("/home/u")),
            Some(PathBuf::from("/data"))
        );
        assert_eq!(
            data_home(os("relative"), os("/home/u")),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        assert_eq!(
            data_home(None, os("/home/u")),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        assert_eq!(data_home(None, None), None);
        assert_eq!(data_home(None, os("relative")), None);
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(shell_quote("/opt/Dash Beam"), "'/opt/Dash Beam'");
        assert_eq!(shell_quote("/it's"), r"'/it'\''s'");
    }

    #[test]
    fn marker_is_on_the_second_line() {
        let script = render_script(&Launcher::Executable("/usr/bin/DashBeam".into()));
        assert!(script.starts_with("#!/bin/sh\n"));
        assert_eq!(script.lines().nth(1), Some(MARKER));
    }

    #[test]
    fn writes_executable_script_and_removes_it() {
        let dir = TempDir::new("roundtrip");
        let scripts = dir.0.join("nautilus").join("scripts");
        let script = render_script(&Launcher::Executable("/usr/bin/DashBeam".into()));

        write_script(&scripts, &script).unwrap();
        let target = scripts.join(SCRIPT_NAME);
        assert!(is_owned_script(&target));
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // Idempotent, and leaves no temp files behind.
        write_script(&scripts, &script).unwrap();
        assert_eq!(fs::read_dir(&scripts).unwrap().count(), 1);

        remove_script(&scripts).unwrap();
        assert!(!target.exists());
        // Removing again is fine.
        remove_script(&scripts).unwrap();
    }

    #[test]
    fn never_touches_a_script_it_did_not_write() {
        let dir = TempDir::new("foreign");
        let target = dir.0.join(SCRIPT_NAME);
        fs::write(&target, "#!/bin/sh\necho mine\n").unwrap();

        let script = render_script(&Launcher::Executable("/usr/bin/DashBeam".into()));
        assert!(write_script(&dir.0, &script).is_err());
        remove_script(&dir.0).unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
    }

    fn run_script(script: &Path, selection: &str) -> std::process::Output {
        run_script_with_path(script, selection, None)
    }

    fn run_script_with_path(
        script: &Path,
        selection: &str,
        path: Option<&Path>,
    ) -> std::process::Output {
        let mut command = Command::new("/bin/sh");
        command
            .arg(script)
            .env("NAUTILUS_SCRIPT_SELECTED_FILE_PATHS", selection);
        if let Some(path) = path {
            command.env("PATH", path);
        }
        command.output().unwrap()
    }

    /// A stand-in `flatpak` whose `info` succeeds only when `installed`, and
    /// which records the arguments of `run` in `args.log` beside it.
    fn fake_flatpak(dir: &Path, installed: bool) {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let log = shell_quote(dir.join("args.log").to_str().unwrap());
        let info_status = if installed { 0 } else { 1 };
        let flatpak = bin.join("flatpak");
        fs::write(
            &flatpak,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n\
                 info) exit {info_status} ;;\n\
                 run) shift; printf '%s\\n' \"$@\" > {log} ;;\n\
                 esac\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&flatpak, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn sh_path(dir: &Path) -> PathBuf {
        // The fake flatpak first, then the system tools the script needs.
        let system = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
        let mut dirs = vec![dir.join("bin")];
        dirs.extend(std::env::split_paths(&system));
        PathBuf::from(std::env::join_paths(dirs).unwrap())
    }

    #[test]
    fn flatpak_script_launches_while_installed() {
        let dir = TempDir::new("flatpak-installed");
        fake_flatpak(&dir.0, true);
        let scripts = dir.0.join("scripts");
        let launcher = Launcher::Flatpak("net.dashbeam.DashBeam".into());
        write_script(&scripts, &render_script(&launcher)).unwrap();
        let script = scripts.join(SCRIPT_NAME);

        let output = run_script_with_path(&script, "/home/u/file\n", Some(&sh_path(&dir.0)));
        assert!(output.status.success(), "{output:?}");
        assert!(script.exists());
        assert_eq!(
            fs::read_to_string(dir.0.join("args.log")).unwrap(),
            "--file-forwarding\nnet.dashbeam.DashBeam\n@@\n/home/u/file\n@@\n"
        );
    }

    #[test]
    fn flatpak_script_removes_itself_once_uninstalled() {
        let dir = TempDir::new("flatpak-orphan");
        fake_flatpak(&dir.0, false);
        let scripts = dir.0.join("scripts");
        let launcher = Launcher::Flatpak("net.dashbeam.DashBeam".into());
        write_script(&scripts, &render_script(&launcher)).unwrap();
        let script = scripts.join(SCRIPT_NAME);

        let output = run_script_with_path(&script, "/home/u/file", Some(&sh_path(&dir.0)));
        assert!(output.status.success(), "{output:?}");
        assert!(!script.exists());
        assert!(!dir.0.join("args.log").exists());
    }

    #[test]
    fn script_launches_with_the_first_selected_path() {
        let dir = TempDir::new("launch");
        // A stand-in for DashBeam that records its arguments.
        let fake = dir.0.join("fake dash'beam");
        let log = dir.0.join("args.log");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
                shell_quote(log.to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let scripts = dir.0.join("scripts");
        write_script(
            &scripts,
            &render_script(&Launcher::Executable(fake.clone())),
        )
        .unwrap();
        let script = scripts.join(SCRIPT_NAME);

        let output = run_script(&script, "/home/u/My File.txt\n/home/u/other\n");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(fs::read_to_string(&log).unwrap(), "/home/u/My File.txt\n");

        // Nothing selected locally: no launch.
        fs::remove_file(&log).unwrap();
        assert!(run_script(&script, "").status.success());
        assert!(!log.exists());
        assert!(script.exists());
    }

    #[test]
    fn script_removes_itself_once_dashbeam_is_gone() {
        let dir = TempDir::new("orphan");
        let scripts = dir.0.join("scripts");
        let missing = dir.0.join("uninstalled").join("DashBeam");
        write_script(&scripts, &render_script(&Launcher::Executable(missing))).unwrap();
        let script = scripts.join(SCRIPT_NAME);

        assert!(run_script(&script, "/home/u/file").status.success());
        assert!(!script.exists());
    }

    #[test]
    fn flatpak_script_forwards_the_file_through_the_portal() {
        let script = render_script(&Launcher::Flatpak("net.dashbeam.DashBeam".into()));
        assert!(script.contains(
            "exec flatpak run --file-forwarding 'net.dashbeam.DashBeam' @@ \"$path\" @@"
        ));
        assert!(script.contains("flatpak info 'net.dashbeam.DashBeam'"));
    }
}
