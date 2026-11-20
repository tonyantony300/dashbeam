import { create } from 'zustand'

/**
 * Phases of a desktop update, in the order they occur. `ready` means the
 * package is downloaded but nothing is installed yet: on Windows the installer
 * quits the app the moment it starts, so `installing` only begins once the
 * user has agreed to restart.
 */
export type UpdaterPhase =
	| 'idle'
	| 'available'
	| 'downloading'
	| 'ready'
	| 'installing'
	| 'restarting'

export interface UpdaterStoreApi {
	phase: () => UpdaterPhase
	version: () => string | null
	/** Release page for builds that cannot install in place; null on desktop. */
	downloadUrl: () => string | null
	downloadedBytes: () => number
	contentLength: () => number | null
	dismissedVersion: () => string | null
	progressRatio: () => number | null
	bannerVisible: () => boolean
	/** Notes carried by the update manifest; often empty. */
	notes: () => string | null
	/** Why the last download or install failed, until retried or dismissed. */
	error: () => string | null
	lastCheckedAt: () => number | null
	updateFound: (
		version: string,
		downloadUrl?: string | null,
		notes?: string | null
	) => void
	noUpdate: () => void
	markChecked: (at: number) => void
	dismiss: () => void
	startDownload: () => boolean
	setContentLength: (bytes: number) => void
	addProgress: (bytes: number) => void
	downloadFinished: () => void
	startInstall: () => boolean
	installFailed: (reason: string) => void
	restarting: () => void
	fail: (reason?: string) => void
}

/**
 * Plain factory holding the update lifecycle, free of zustand/React so it runs
 * under `node:test`. `useUpdaterStore` below wraps one instance for components.
 *
 * It is the single source of truth for all three update surfaces (the banner,
 * the settings sidebar alert and the settings panel), which is what keeps two
 * of them from starting two downloads of the same release.
 */
export function createUpdaterStore(
	options: {
		dismissedVersion?: string | null
		lastCheckedAt?: number | null
	} = {}
): UpdaterStoreApi {
	let phase: UpdaterPhase = 'idle'
	let version: string | null = null
	let downloadUrl: string | null = null
	let downloadedBytes = 0
	let contentLength: number | null = null
	let dismissedVersion: string | null = options.dismissedVersion ?? null
	let notes: string | null = null
	let error: string | null = null
	let lastCheckedAt: number | null = options.lastCheckedAt ?? null

	const clearProgress = () => {
		downloadedBytes = 0
		contentLength = null
	}

	const updateFound: UpdaterStoreApi['updateFound'] = (
		found,
		url = null,
		foundNotes = null
	) => {
		// A periodic re-check must not restart a download that's already running.
		if (phase !== 'idle' && phase !== 'available') return
		// A failure belongs to the release it happened on.
		if (found !== version) error = null
		version = found
		downloadUrl = url
		notes = foundNotes?.trim() ? foundNotes : null
		phase = 'available'
	}

	const noUpdate: UpdaterStoreApi['noUpdate'] = () => {
		if (phase !== 'idle' && phase !== 'available') return
		version = null
		downloadUrl = null
		notes = null
		error = null
		phase = 'idle'
		clearProgress()
	}

	const startDownload: UpdaterStoreApi['startDownload'] = () => {
		if (phase !== 'available') return false
		phase = 'downloading'
		error = null
		clearProgress()
		return true
	}

	const fail: UpdaterStoreApi['fail'] = (reason) => {
		phase = version ? 'available' : 'idle'
		error = version ? (reason ?? null) : null
		clearProgress()
	}

	return {
		phase: () => phase,
		version: () => version,
		downloadUrl: () => downloadUrl,
		downloadedBytes: () => downloadedBytes,
		contentLength: () => contentLength,
		dismissedVersion: () => dismissedVersion,
		progressRatio: () =>
			contentLength ? Math.min(1, downloadedBytes / contentLength) : null,
		// "Later" only silences the banner for the version it was clicked on, and
		// only while that version is still merely available — once bytes are
		// moving the banner is progress, not a nag. A failure always shows:
		// the user asked for this update and must hear that it didn't happen.
		bannerVisible: () =>
			phase !== 'idle' &&
			(phase !== 'available' || dismissedVersion !== version || error !== null),
		notes: () => notes,
		error: () => error,
		lastCheckedAt: () => lastCheckedAt,
		updateFound,
		noUpdate,
		markChecked: (at) => {
			lastCheckedAt = at
		},
		dismiss: () => {
			dismissedVersion = version
			error = null
		},
		startDownload,
		setContentLength: (bytes) => {
			contentLength = bytes
		},
		addProgress: (bytes) => {
			downloadedBytes += bytes
		},
		downloadFinished: () => {
			if (phase === 'downloading') phase = 'ready'
		},
		// Whichever surface clicks first owns the install; the rest no-op.
		startInstall: () => {
			if (phase !== 'ready') return false
			phase = 'installing'
			error = null
			return true
		},
		// The package is still on disk, so the user can retry from `ready`.
		installFailed: (reason) => {
			phase = 'ready'
			error = reason
		},
		restarting: () => {
			phase = 'restarting'
		},
		fail,
	}
}

const DISMISSED_KEY = 'dashbeam.updater.dismissed-version'
const LAST_CHECKED_KEY = 'dashbeam.updater.last-checked-at'

/** `localStorage` is absent under `node --test`, and can throw in a webview. */
function readStored(key: string): string | null {
	try {
		return typeof localStorage === 'undefined'
			? null
			: localStorage.getItem(key)
	} catch {
		return null
	}
}

function writeStored(key: string, value: string | null): void {
	try {
		if (typeof localStorage === 'undefined') return
		if (value === null) localStorage.removeItem(key)
		else localStorage.setItem(key, value)
	} catch {
		// Losing this across a restart is not worth failing over.
	}
}

function readLastCheckedAt(): number | null {
	const stored = Number(readStored(LAST_CHECKED_KEY))
	return Number.isFinite(stored) && stored > 0 ? stored : null
}

/** Which release the "What's new" dialog is showing, and from which side. */
export type WhatsNewTarget = {
	version: string
	/** True after updating; false while the release is still only offered. */
	installed: boolean
}

type UpdaterState = {
	phase: UpdaterPhase
	version: string | null
	downloadUrl: string | null
	downloadedBytes: number
	contentLength: number | null
	dismissedVersion: string | null
	progressRatio: number | null
	bannerVisible: boolean
	notes: string | null
	error: string | null
	lastCheckedAt: number | null
	whatsNew: WhatsNewTarget | null
	updateFound: UpdaterStoreApi['updateFound']
	noUpdate: () => void
	markChecked: (at: number) => void
	dismiss: () => void
	startDownload: () => boolean
	setContentLength: (bytes: number) => void
	addProgress: (bytes: number) => void
	downloadFinished: () => void
	startInstall: () => boolean
	installFailed: (reason: string) => void
	restarting: () => void
	fail: (reason?: string) => void
	openWhatsNew: (target: WhatsNewTarget) => void
	closeWhatsNew: () => void
}

/** Every derived value comes from the core, so the tested logic has one home. */
function snapshot(core: UpdaterStoreApi) {
	return {
		phase: core.phase(),
		version: core.version(),
		downloadUrl: core.downloadUrl(),
		downloadedBytes: core.downloadedBytes(),
		contentLength: core.contentLength(),
		dismissedVersion: core.dismissedVersion(),
		progressRatio: core.progressRatio(),
		bannerVisible: core.bannerVisible(),
		notes: core.notes(),
		error: core.error(),
		lastCheckedAt: core.lastCheckedAt(),
	}
}

export const useUpdaterStore = create<UpdaterState>((set) => {
	const core = createUpdaterStore({
		dismissedVersion: readStored(DISMISSED_KEY),
		lastCheckedAt: readLastCheckedAt(),
	})
	const publish = () => set(snapshot(core))
	// The updater emits one Progress event per chunk — thousands for a typical
	// installer. Re-rendering on each would be its own kind of jerk, so the core
	// stays byte-exact while the UI only hears about visible movement.
	let publishedRatio: number | null = null

	return {
		...snapshot(core),
		whatsNew: null,
		openWhatsNew: (target) => set({ whatsNew: target }),
		closeWhatsNew: () => set({ whatsNew: null }),
		updateFound: (version, downloadUrl, notes) => {
			core.updateFound(version, downloadUrl, notes)
			publish()
		},
		noUpdate: () => {
			core.noUpdate()
			publish()
		},
		markChecked: (at) => {
			if (core.lastCheckedAt() === at) return
			core.markChecked(at)
			writeStored(LAST_CHECKED_KEY, String(at))
			publish()
		},
		dismiss: () => {
			core.dismiss()
			writeStored(DISMISSED_KEY, core.dismissedVersion())
			publish()
		},
		startDownload: () => {
			const started = core.startDownload()
			if (started) {
				publishedRatio = null
				publish()
			}
			return started
		},
		setContentLength: (bytes) => {
			core.setContentLength(bytes)
			publish()
		},
		addProgress: (bytes) => {
			core.addProgress(bytes)
			const ratio = core.progressRatio()
			// Without a content length nothing on screen moves, so a byte count
			// nobody renders isn't worth a render.
			if (ratio === null) return
			if (
				publishedRatio !== null &&
				ratio - publishedRatio < 0.005 &&
				ratio < 1
			) {
				return
			}
			publishedRatio = ratio
			publish()
		},
		downloadFinished: () => {
			core.downloadFinished()
			publish()
		},
		startInstall: () => {
			const started = core.startInstall()
			if (started) publish()
			return started
		},
		installFailed: (reason) => {
			core.installFailed(reason)
			publish()
		},
		restarting: () => {
			core.restarting()
			publish()
		},
		fail: (reason) => {
			core.fail(reason)
			publishedRatio = null
			publish()
		},
	}
})
