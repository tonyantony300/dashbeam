use tauri::{AppHandle, Manager};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    path::BaseDirectory,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use tauri::menu::ContextMenu;

// to show confirmation dialog box for quit event from tray
// use tauri_plugin_dialog::DialogExt;

static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mirrors the frontend's `minimizeToTray` setting. Not in `AppState` because
/// the window-close handler is synchronous and can't lock `AppStateMutex`.
static BACKGROUND_ON_CLOSE: AtomicBool = AtomicBool::new(true);

pub fn is_active() -> bool {
    TRAY_ACTIVE.load(Ordering::Relaxed)
}

/// Windows/Linux only — macOS hides on close unconditionally.
#[cfg(not(target_os = "macos"))]
pub fn background_on_close() -> bool {
    BACKGROUND_ON_CLOSE.load(Ordering::Relaxed)
}

pub fn set_background_on_close(enabled: bool) {
    BACKGROUND_ON_CLOSE.store(enabled, Ordering::Relaxed);
}

/// Tray strings, pushed from the frontend so the menu follows the app language.
/// English defaults cover startup, before the first `set_tray_labels` call.
#[derive(Clone, serde::Deserialize)]
pub struct TrayLabels {
    pub open: String,
    pub quit: String,
    pub no_devices: String,
    /// Template with `{{online}}` and `{{total}}` placeholders.
    pub devices_online: String,
    /// Template with `{{name}}` for each online device row.
    pub device_online: String,
}

impl Default for TrayLabels {
    fn default() -> Self {
        Self {
            open: "Open".to_string(),
            quit: "Quit".to_string(),
            no_devices: "No paired devices".to_string(),
            devices_online: "{{online}} of {{total}} devices online".to_string(),
            device_online: "{{name}} - Online".to_string(),
        }
    }
}

pub fn format_presence(labels: &TrayLabels, online: usize, total: usize) -> String {
    if total == 0 {
        return labels.no_devices.clone();
    }
    labels
        .devices_online
        .replace("{{online}}", &online.to_string())
        .replace("{{total}}", &total.to_string())
}

/// Soft cap for a tray row's name. Native menus don't wrap, so a 64-char
/// display name would set the whole menu's width. Leaves room for " - Online".
const TRAY_DEVICE_NAME_MAX_CHARS: usize = 36;

fn truncate_device_name(name: &str) -> String {
    let count = name.chars().count();
    if count <= TRAY_DEVICE_NAME_MAX_CHARS {
        return name.to_string();
    }
    let keep = TRAY_DEVICE_NAME_MAX_CHARS.saturating_sub(1);
    let mut out: String = name.chars().take(keep).collect();
    out.push('…');
    out
}

fn format_device_online_row(labels: &TrayLabels, name: &str) -> String {
    labels
        .device_online
        .replace("{{name}}", &truncate_device_name(name))
}

/// Active+online display names, sorted case-insensitively.
fn sorted_online_names(devices: &[engine::PairedDeviceInfo]) -> Vec<String> {
    let mut names: Vec<String> = devices
        .iter()
        .filter(|d| d.pairing_status.is_active() && d.online)
        .map(|d| d.display_name.clone())
        .collect();
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    names
}

/// Return true if window was shown (or attempted) successfully, false otherwise.
pub fn open_and_focus(app: &AppHandle) -> bool {
    // try main window by label first
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.show() {
            tracing::warn!("Failed to show window: {}", e);
            return false;
        }
        if let Err(e) = window.set_focus() {
            tracing::warn!("Failed to set focus on window: {}", e);
        }
        return true;
    }

    // fallback: use the first available webview window
    if let Some((_label, window)) = app.webview_windows().iter().next() {
        if let Err(e) = window.show() {
            tracing::warn!("Failed to show fallback window: {}", e);
            return false;
        }
        if let Err(e) = window.set_focus() {
            tracing::warn!("Failed to set focus on fallback window: {}", e);
        }
        return true;
    }

    tracing::error!("No window available to show");
    false
}

#[derive(Clone, Default)]
struct PresenceState {
    generation: u64,
    online: usize,
    total: usize,
    online_names: Vec<String>,
}

/// Everything needed to mutate the tray after `setup_tray` returns.
pub struct TrayHandles {
    pub tray: tauri::tray::TrayIcon,
    pub menu: Menu<tauri::Wry>,
    pub status: MenuItem<tauri::Wry>,
    /// Disabled per-device rows currently in the menu (between status and separator).
    pub device_items: Mutex<Vec<MenuItem<tauri::Wry>>>,
    pub open: MenuItem<tauri::Wry>,
    pub quit: MenuItem<tauri::Wry>,
    pub labels: Mutex<TrayLabels>,
    /// Last known presence snapshot. Generation shares the lock with the counts
    /// so a stale refresh can't overwrite a fresher one — see `refresh_presence`.
    presence: Mutex<PresenceState>,
    /// Monotonic generation handed to each `refresh_presence` in request order;
    /// refreshes race on a blocking file read and can finish out of order.
    pub next_generation: AtomicU64,
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn popup_tray_menu(app: &AppHandle, menu: &Menu<tauri::Wry>) {
    let Some(window) = app.get_webview_window("main").or_else(|| {
        app.webview_windows()
            .into_iter()
            .next()
            .map(|(_, window)| window)
    }) else {
        tracing::warn!("No window available to anchor tray menu");
        return;
    };
    if let Err(error) = menu.popup(window.as_ref().window()) {
        tracing::warn!("Failed to show tray menu: {error}");
    }
}

pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let labels = TrayLabels::default();
    // Disabled: a status readout, not an action.
    let status = MenuItem::with_id(app, "status", &labels.no_devices, false, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let open = MenuItem::with_id(app, "open", &labels.open, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", &labels.quit, true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&status, &separator, &open, &quit])?;

    // macOS/Windows: attach the context menu so the platform opens it natively.
    // On Windows left-click focuses the window (left = primary action, right =
    // context menu), so left-click must not open the menu. macOS keeps its native
    // left-click-to-open-menu behavior, so there only right-click focuses the window.
    #[cfg(target_os = "windows")]
    let show_menu_on_left_click = false;
    #[cfg(target_os = "macos")]
    let show_menu_on_left_click = true;

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .show_menu_on_left_click(show_menu_on_left_click)
        .tooltip("DashBeam")
        .on_tray_icon_event(move |tray, event| {
            #[cfg(target_os = "windows")]
            let focus_button = MouseButton::Left;
            #[cfg(target_os = "macos")]
            let focus_button = MouseButton::Right;
            if let TrayIconEvent::Click {
                button,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if button == focus_button {
                    let _ = open_and_focus(tray.app_handle());
                }
            }
        });

    // Linux: the appindicator host only shows the item once it has a menu, and
    // click events never reach the app there, so attach the menu up front.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut builder = {
        let menu_for_click = menu.clone();
        TrayIconBuilder::new()
            .menu(&menu)
            .tooltip("DashBeam")
            .on_tray_icon_event(move |tray, event| {
                let TrayIconEvent::Click {
                    button,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                else {
                    return;
                };
                let app = tray.app_handle();
                match button {
                    MouseButton::Left => popup_tray_menu(app, &menu_for_click),
                    MouseButton::Right => {
                        let _ = open_and_focus(app);
                    }
                    _ => {}
                }
            })
    };

    builder = builder.on_menu_event(move |app, event| match event.id().as_ref() {
        "open" => {
            open_and_focus(app);
        }
        "quit" => {
            tracing::info!("Quit requested from tray");

            app.exit(0);
        }
        // Online device rows are enabled for appearance only.
        id if id.starts_with("tray-online-") => {}
        _ => {}
    });

    // macOS: dedicated monochrome ring template (transparent, no square fill).
    // Windows: centre-cropped colour mark so the rings fill ~2× more of the
    // notification-area slot than the full app icon (which has heavy padding).
    // Linux: full colour app icon.
    #[cfg(target_os = "macos")]
    let tray_icon_resource = "icons/tray-template.png";
    #[cfg(target_os = "windows")]
    let tray_icon_resource = "icons/tray-windows.png";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let tray_icon_resource = "icons/128x128.png";

    let icon = match app
        .path()
        .resolve(tray_icon_resource, BaseDirectory::Resource)
        .ok()
        .and_then(|p| tauri::image::Image::from_path(&p).ok())
    {
        Some(img) => img,
        None => {
            tracing::warn!(
                resource = tray_icon_resource,
                "Could not load tray icon, falling back to default window icon"
            );
            app.default_window_icon().cloned().ok_or_else(|| {
                tauri::Error::InvalidIcon(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "tray icon not found",
                ))
            })?
        }
    };

    builder = builder.icon(icon);
    // macOS menu bar items must be NSImage templates: the system renders them
    // as a mask so they invert against light/dark menu bars and stay legible
    // under Reduce Transparency.
    #[cfg(target_os = "macos")]
    {
        builder = builder.icon_as_template(true);
    }
    let tray = builder.build(app)?;

    app.manage(TrayHandles {
        tray,
        menu,
        status,
        device_items: Mutex::new(Vec::new()),
        open,
        quit,
        labels: Mutex::new(labels),
        presence: Mutex::new(PresenceState::default()),
        next_generation: AtomicU64::new(0),
    });
    TRAY_ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}

/// Re-render summary, online-device rows, Open/Quit labels, and tooltip.
fn render(app: &AppHandle, handles: &TrayHandles) {
    let labels = handles.labels.lock().expect("tray labels lock").clone();
    let presence = handles.presence.lock().expect("tray presence lock").clone();
    let status = format_presence(&labels, presence.online, presence.total);
    let _ = handles.status.set_text(&status);
    let _ = handles.open.set_text(&labels.open);
    let _ = handles.quit.set_text(&labels.quit);
    let _ = handles
        .tray
        .set_tooltip(Some(format!("DashBeam - {status}")));

    let mut device_items = handles.device_items.lock().expect("tray device items lock");
    for item in device_items.drain(..) {
        let _ = handles.menu.remove(&item);
    }
    for (index, name) in presence.online_names.iter().enumerate() {
        let id = format!("tray-online-{index}");
        let title = format_device_online_row(&labels, name);
        // Enabled so the row isn't greyed out; clicks are ignored in on_menu_event.
        match MenuItem::with_id(app, &id, &title, true, None::<&str>) {
            Ok(item) => {
                // After status (0); each insert shifts later items down.
                if let Err(error) = handles.menu.insert(&item, 1 + index) {
                    tracing::warn!(%error, "Failed to insert tray device row");
                    break;
                }
                device_items.push(item);
            }
            Err(error) => {
                tracing::warn!(%error, "Failed to create tray device row");
                break;
            }
        }
    }
}

/// Swap in translated strings; called by the frontend on mount and whenever
/// the app language changes.
pub fn apply_labels(app: &AppHandle, labels: TrayLabels) {
    let Some(handles) = app.try_state::<TrayHandles>() else {
        return;
    };
    *handles.labels.lock().expect("tray labels lock") = labels;
    render(app, &handles);
}

/// Write a presence snapshot into `current` unless a newer generation already
/// landed there. Returns whether the write happened. Kept free of Tauri/async
/// types so the ordering guarantee is unit-testable on its own.
fn apply_if_newer(
    current: &mut PresenceState,
    generation: u64,
    online: usize,
    total: usize,
    online_names: Vec<String>,
) -> bool {
    if generation <= current.generation {
        return false;
    }
    current.generation = generation;
    current.online = online;
    current.total = total;
    current.online_names = online_names;
    true
}

/// Recompute presence from the node and re-render. Cheap enough to call on
/// every presence event — `list_paired` is a store read, not a network call.
///
/// Each call spawns a task doing a blocking file read, so tasks can finish out
/// of order. A generation number handed out synchronously in call order, plus
/// `apply_if_newer`, keeps the tray on the most recently *requested* refresh.
pub fn refresh_presence(app: &AppHandle) {
    let app = app.clone();
    let Some(handles) = app.try_state::<TrayHandles>() else {
        return;
    };
    let generation = handles.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
    drop(handles);

    tauri::async_runtime::spawn(async move {
        let node = {
            let state = app.state::<crate::state::AppStateMutex>();
            let guard = state.lock().await;
            guard.node.clone()
        };
        let Some(node) = node else {
            return;
        };
        let Ok(devices) = node.list_paired() else {
            return;
        };
        let online_names = sorted_online_names(&devices);
        let online = online_names.len();
        let total = devices
            .iter()
            .filter(|d| d.pairing_status.is_active())
            .count();
        let Some(handles) = app.try_state::<TrayHandles>() else {
            return;
        };
        let wrote = {
            let mut presence = handles.presence.lock().expect("tray presence lock");
            apply_if_newer(&mut presence, generation, online, total, online_names)
        };
        if wrote {
            render(&app, &handles);
        }
    });
}

#[cfg(test)]
mod refresh_ordering_tests {
    use super::{apply_if_newer, PresenceState};

    #[test]
    fn later_request_wins_even_when_it_finishes_first() {
        let mut presence = PresenceState::default();
        // Generation 2 (the later request) finishes first...
        assert!(apply_if_newer(
            &mut presence,
            2,
            2,
            3,
            vec!["A".into(), "B".into()]
        ));
        assert_eq!(presence.generation, 2);
        assert_eq!(presence.online_names, vec!["A", "B"]);
        // ...generation 1 finishes after, and must not clobber it.
        assert!(!apply_if_newer(&mut presence, 1, 0, 1, vec!["Z".into()]));
        assert_eq!(presence.online_names, vec!["A", "B"]);
    }

    #[test]
    fn stale_generation_is_dropped_not_applied() {
        let mut presence = PresenceState {
            generation: 5,
            online: 1,
            total: 3,
            online_names: vec!["Keep".into()],
        };
        assert!(!apply_if_newer(
            &mut presence,
            3,
            9,
            9,
            vec!["Stale".into()]
        ));
        assert_eq!(presence.online_names, vec!["Keep"]);
    }

    #[test]
    fn equal_generation_is_not_treated_as_newer() {
        let mut presence = PresenceState {
            generation: 4,
            online: 1,
            total: 1,
            online_names: vec!["Keep".into()],
        };
        assert!(!apply_if_newer(&mut presence, 4, 9, 9, vec!["Nope".into()]));
        assert_eq!(presence.online_names, vec!["Keep"]);
    }

    #[test]
    fn strictly_increasing_generations_each_apply() {
        let mut presence = PresenceState::default();
        for gen in 1..=5u64 {
            assert!(apply_if_newer(
                &mut presence,
                gen,
                gen as usize,
                5,
                vec![format!("d{gen}")]
            ));
        }
        assert_eq!(presence.generation, 5);
        assert_eq!(presence.online_names, vec!["d5"]);
    }
}

#[cfg(test)]
mod presence_label_tests {
    use super::{format_presence, TrayLabels};

    fn labels() -> TrayLabels {
        TrayLabels {
            open: "Open".to_string(),
            quit: "Quit".to_string(),
            no_devices: "No paired devices".to_string(),
            devices_online: "{{online}} of {{total}} devices online".to_string(),
            device_online: "{{name}} - Online".to_string(),
        }
    }

    #[test]
    fn substitutes_both_counts() {
        assert_eq!(format_presence(&labels(), 2, 3), "2 of 3 devices online");
    }

    #[test]
    fn uses_the_empty_label_when_nothing_is_paired() {
        assert_eq!(format_presence(&labels(), 0, 0), "No paired devices");
    }

    #[test]
    fn leaves_unknown_placeholders_untouched() {
        let mut l = labels();
        l.devices_online = "{{online}}/{{total}} ({{bogus}})".to_string();
        assert_eq!(format_presence(&l, 1, 4), "1/4 ({{bogus}})");
    }
}

#[cfg(test)]
mod device_row_label_tests {
    use super::{
        format_device_online_row, truncate_device_name, TrayLabels, TRAY_DEVICE_NAME_MAX_CHARS,
    };

    fn labels() -> TrayLabels {
        TrayLabels::default()
    }

    #[test]
    fn formats_name_with_online_suffix() {
        assert_eq!(
            format_device_online_row(&labels(), "Pixel 8"),
            "Pixel 8 - Online"
        );
    }

    #[test]
    fn truncates_long_names_but_keeps_online_suffix() {
        let long = "A".repeat(64);
        let label = format_device_online_row(&labels(), &long);
        assert!(label.ends_with(" - Online"));
        assert!(label.contains('…'));
        let name_part = label.strip_suffix(" - Online").unwrap();
        assert_eq!(name_part.chars().count(), TRAY_DEVICE_NAME_MAX_CHARS);
    }

    #[test]
    fn truncate_leaves_short_names_alone() {
        assert_eq!(truncate_device_name("MacBook"), "MacBook");
    }
}

#[cfg(test)]
mod online_names_tests {
    use super::sorted_online_names;
    use engine::{PairedDeviceInfo, PairingStatus};

    fn device(name: &str, online: bool, status: PairingStatus) -> PairedDeviceInfo {
        PairedDeviceInfo {
            endpoint_id: name.to_string(),
            display_name: name.to_string(),
            device_type: "laptop".to_string(),
            os: "macos".to_string(),
            paired_at: 0,
            last_seen_at: 0,
            relay_url: None,
            pairing_status: status,
            trusted: false,
            online,
        }
    }

    #[test]
    fn lists_only_active_online_sorted_case_insensitively() {
        let devices = vec![
            device("zeta", true, PairingStatus::Active),
            device("Alpha", true, PairingStatus::Active),
            device("offline", false, PairingStatus::Active),
            device("gone", true, PairingStatus::UnpairedRemotely),
        ];
        assert_eq!(
            sorted_online_names(&devices),
            vec!["Alpha".to_string(), "zeta".to_string()]
        );
    }

    #[test]
    fn empty_when_nobody_is_online() {
        let devices = vec![device("only-offline", false, PairingStatus::Active)];
        assert!(sorted_online_names(&devices).is_empty());
    }
}
