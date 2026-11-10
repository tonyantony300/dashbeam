// VITE_APP_PLATFORM is a build-time hint (tauri vs web).
// TAURI_PLATFORM is injected by Vite define from TAURI_ENV_PLATFORM (OS target when on Tauri).
// Runtime detection wins: a plain browser must never call Tauri APIs even if the wrong
// dev script or env file was used (e.g. opening the Tauri Vite port in Safari).
//
// `import.meta.env` is undefined under plain Node (lib unit tests). Vite replaces
// these identifiers at build time; optional chaining keeps the Node path safe.
const appPlatform = import.meta.env?.VITE_APP_PLATFORM ?? ''
const platform = import.meta.env?.TAURI_PLATFORM ?? ''

function isTauriRuntime(): boolean {
	if (typeof window === 'undefined') {
		return appPlatform === 'tauri'
	}

	const w = window as Window & {
		__TAURI_INTERNALS__?: unknown
		__TAURI__?: unknown
	}

	return w.__TAURI_INTERNALS__ != null || w.__TAURI__ != null
}

export const IS_TAURI = isTauriRuntime()
export const IS_WEB = !IS_TAURI
export const IS_ANDROID = IS_TAURI && platform.includes('android')
export const IS_IOS = IS_TAURI && platform.includes('ios')
export const IS_MOBILE = IS_ANDROID || IS_IOS
export const IS_MACOS = IS_TAURI && platform.includes('darwin')
export const IS_WINDOWS = IS_TAURI && platform.includes('windows')
export const IS_LINUX = IS_TAURI && platform.includes('linux')
export const IS_DESKTOP =
	IS_TAURI && !IS_MOBILE && (IS_MACOS || IS_WINDOWS || IS_LINUX)
/** Persistent pairing node (host/join/invite) — every native app, not web. */
export const IS_PAIRING_CAPABLE = IS_DESKTOP || IS_MOBILE
/**
 * Nearby (mDNS LAN discovery) and the discoverability setting. Not iOS: iOS
 * drops the raw multicast socket mDNS uses unless the app holds the
 * Apple-approved com.apple.developer.networking.multicast entitlement. The
 * engine forces discoverability off there too (`effective_discoverability`).
 */
export const IS_NEARBY_CAPABLE = IS_PAIRING_CAPABLE && !IS_IOS

export const IS_FLATPAK = IS_TAURI && import.meta.env.VITE_IS_FLATPAK === 'true'

/**
 * Android APKs are sideloaded from GitHub releases, so those builds check for
 * a newer one and hand the user to the release page — they cannot install it
 * themselves. Opt-in per build, because a Play Store build must not do this:
 * Play forbids an app updating itself outside Play's own mechanism, and the
 * Play copy is signed by a different key, so its users could not install the
 * GitHub APK over it anyway. Only the APK release job sets this.
 */
export const IS_ANDROID_UPDATE_CHECK_ENABLED =
	IS_ANDROID && import.meta.env.VITE_ANDROID_UPDATE_CHECK === 'true'

/**
 * Whether this build has an in-app updater at all. Flatpak updates through
 * `flatpak update` and a Play build through Play, so neither has anything to
 * check. The periodic check and every surface that displays it share this, so
 * they cannot drift — a check running with nothing to show it is how the
 * Android updater ended up unreachable.
 *
 * Windows portable is excluded too, but only at runtime (`useIsWindowsPortable`).
 */
export const IS_UPDATER_AVAILABLE =
	(IS_DESKTOP && !IS_FLATPAK) || IS_ANDROID_UPDATE_CHECK_ENABLED
