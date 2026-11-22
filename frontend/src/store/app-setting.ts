import { create } from 'zustand'
import { createJSONStorage, persist } from 'zustand/middleware'
import {
	MEDIA_STORE_DEFAULT_VERSION,
	migrateDownloadFolder,
} from '../lib/download-folder-migration'
import { IS_DESKTOP } from '../lib/platform'
import {
	defaultAppSettings,
	localSettingLazyStoreStorage,
} from '../lib/setting-store'

export type AppSettingsState = {
	minimizeToTray: boolean
	startOnBoot: boolean
	enableNotifications: boolean
	darkMode: boolean
	autoUpdate: boolean
	showProgressOnIcon: boolean
	downloadsPath: string
	downloadsUri: string
	/**
	 * File manager right-click entry: the Explorer verb on Windows, the GNOME
	 * Files (Nautilus) script on Linux. Key kept from the Windows-only days so
	 * existing settings files keep working.
	 */
	windowsContextMenu: boolean
	relayMode: 'default' | 'custom' | 'disabled'
	relayUrls: string[]
	relayAuthToken: string
	relayFallback: 'strict' | 'public'
	discoveryMode: 'default' | 'custom'
	pkarrRelayUrl: string
	dnsOrigin: string
	showBroadcastToggle: boolean
	/**
	 * Whether the footer shows the relay status button. Off by default — the
	 * relay endpoint is plumbing most users never need to see.
	 */
	showRelayStatus: boolean
	/**
	 * Nearby/LAN discoverability, mirroring the engine's `Discoverability`.
	 * `init_node_service` reads this store's file before the node starts, so an
	 * `Off` choice never registers mDNS even briefly; `DeviceNodeSync` re-applies
	 * it once the node is ready, as a safety net.
	 */
	discoverability: 'everyone' | 'paired-only' | 'off'
	/**
	 * One-shot: the first-run "enable autostart" step has run. Records that we
	 * tried, not that it succeeded — neither a user who turned it back off nor a
	 * platform that refused should be asked again.
	 */
	autostartInitialized: boolean
	/**
	 * Whether finished transfers are written to the history page. Read Rust-side
	 * at recorder construction, so turning it off stops new rows immediately.
	 * Existing rows are left alone — disabling is not clearing.
	 */
	enableTransferHistory: boolean
}

export type AppSettingsActions = {
	setMinimizeToTray: (value: boolean) => void
	setStartOnBoot: (value: boolean) => void
	setEnableNotifications: (value: boolean) => void
	setDarkMode: (value: boolean) => void
	setAutoUpdate: (value: boolean) => void
	toggleShowProgressOnIcon?: (value: boolean) => void
	setDownloadsPath: (value: string) => void
	setDownloadsUri: (value: string) => void
	setWindowsContextMenu: (value: boolean) => void
	setRelayMode: (value: 'default' | 'custom' | 'disabled') => void
	setRelayUrls: (value: string[]) => void
	setRelayAuthToken: (value: string) => void
	setRelayFallback: (value: 'strict' | 'public') => void
	setDiscoveryMode: (value: 'default' | 'custom') => void
	setPkarrRelayUrl: (value: string) => void
	setDnsOrigin: (value: string) => void
	setShowBroadcastToggle: (value: boolean) => void
	setShowRelayStatus: (value: boolean) => void
	setDiscoverability: (value: 'everyone' | 'paired-only' | 'off') => void
	setAutostartInitialized: (value: boolean) => void
	setEnableTransferHistory: (value: boolean) => void
}

export type AppSettings = AppSettingsState & AppSettingsActions

const AppSettingsKey = 'app_settings'

/** Bumped whenever a persisted value needs correcting; see `migrateAppSettings`. */
const AppSettingsVersion = MEDIA_STORE_DEFAULT_VERSION

/**
 * v0 → v1: force `minimizeToTray` back to `true`. Nothing read this key until
 * the always-on-presence work landed, but `persist` has no `partialize`, so the
 * dead `false` default was written to disk the first time any setting changed.
 * Reading it as intent would turn "close hides to tray" into "close quits",
 * killing in-flight transfers on the first upgrade.
 *
 * v1 → v2: drop the stored Android download folder — see `migrateDownloadFolder`.
 *
 * Every other persisted value carries through untouched, and the new version is
 * written back during rehydration, so each step runs at most once per install.
 */
function migrateAppSettings(
	persistedState: unknown,
	version: number
): Partial<AppSettingsState> {
	// `persistedState` is whatever happened to be on disk — never assume a shape.
	const state =
		typeof persistedState === 'object' &&
		persistedState !== null &&
		!Array.isArray(persistedState)
			? (persistedState as Partial<AppSettingsState>)
			: {}

	const withDownloadFolder = migrateDownloadFolder(state, version)

	if (version < 1) {
		return { ...withDownloadFolder, minimizeToTray: true }
	}
	return withDownloadFolder
}

export const useAppSettingStore = create<AppSettings>()(
	persist(
		(set) => ({
			...defaultAppSettings,
			setMinimizeToTray: (value: boolean) => set({ minimizeToTray: value }),
			setStartOnBoot: (value: boolean) => set({ startOnBoot: value }),
			setEnableNotifications: (value: boolean) =>
				set({ enableNotifications: value }),
			setDarkMode: (value: boolean) => set({ darkMode: value }),
			setAutoUpdate: (value: boolean) => set({ autoUpdate: value }),
			toggleShowProgressOnIcon: (value: boolean) =>
				set({ showProgressOnIcon: value }),
			setDownloadsPath: (value: string) => set({ downloadsPath: value }),
			setDownloadsUri: (value: string) => set({ downloadsUri: value }),
			setWindowsContextMenu: (value: boolean) =>
				set({ windowsContextMenu: value }),
			setRelayMode: (value: 'default' | 'custom' | 'disabled') =>
				set({ relayMode: value }),
			setRelayUrls: (value: string[]) => set({ relayUrls: value }),
			setRelayAuthToken: (value: string) => set({ relayAuthToken: value }),
			setRelayFallback: (value: 'strict' | 'public') =>
				set({ relayFallback: value }),
			setDiscoveryMode: (value: 'default' | 'custom') =>
				set({ discoveryMode: value }),
			setPkarrRelayUrl: (value: string) => set({ pkarrRelayUrl: value }),
			setDnsOrigin: (value: string) => set({ dnsOrigin: value }),
			setShowBroadcastToggle: (value: boolean) =>
				set({ showBroadcastToggle: value }),
			setShowRelayStatus: (value: boolean) => set({ showRelayStatus: value }),
			setDiscoverability: (value: 'everyone' | 'paired-only' | 'off') =>
				set({ discoverability: value }),
			setAutostartInitialized: (value: boolean) =>
				set({ autostartInitialized: value }),
			setEnableTransferHistory: (value: boolean) =>
				set({ enableTransferHistory: value }),
		}),
		{
			name: AppSettingsKey,
			version: AppSettingsVersion,
			migrate: migrateAppSettings,
			storage: createJSONStorage(() => localSettingLazyStoreStorage),
			merge: (persistedState, currentState) => ({
				...currentState,
				...(persistedState as Partial<AppSettings>),
			}),
			// The Rust close handler seeds itself from `settings.json` during
			// `setup()`, before the webview rehydrates — push the hydrated value
			// across so a migration applies to this session, not just the next.
			onRehydrateStorage: () => (state) => {
				if (!state || !IS_DESKTOP) return
				void import('../lib/platform-api')
					.then(({ invoke }) =>
						invoke('set_background_on_close', {
							enabled: state.minimizeToTray,
						})
					)
					.catch((error) => {
						console.warn('Failed to sync minimizeToTray to backend:', error)
					})
			},
		}
	)
)
