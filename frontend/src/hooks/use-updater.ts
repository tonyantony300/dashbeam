import {
	queryOptions,
	type UseQueryOptions,
	useMutation,
	useQuery,
	useQueryClient,
} from '@tanstack/react-query'
import { useCallback, useEffect, useState } from 'react'
import { relaunch } from '@tauri-apps/plugin-process'
import { check } from '@tauri-apps/plugin-updater'
import { toastManager } from '../components/ui/toast'
import { useTranslation } from '../i18n/react-i18next-compat'
import {
	IS_ANDROID_UPDATE_CHECK_ENABLED,
	IS_MOBILE,
	IS_WEB,
	IS_FLATPAK,
} from '../lib/platform'
import { invoke } from '../lib/platform-api'
import { openExternalUrl } from '../lib/openExternalUrl'
import { fetchReleaseNotes, shouldAnnounceUpdate } from '../lib/release-notes'
import { VERSION } from '../lib/version'
import { useUpdaterStore } from '../store/updater-store'
import { useTransferBusy } from './use-transfer-busy'
import { isWindowsPortableBuild } from './use-windows-portable'

type TauriUpdate = NonNullable<Awaited<ReturnType<typeof check>>>

/**
 * What a sideloaded Android check yields: there is nothing to install, only a
 * release page to open. Mirrors the `AndroidUpdate` the Rust command returns.
 */
type AndroidUpdate = {
	version: string
	notes: string
	url: string
}

/**
 * One shape for both paths. `handle` is the plugin's `Update`, present only
 * where the app can install the release itself; `url` is set instead where it
 * can only point at one.
 */
export type UpdateInfo = {
	version: string
	handle: TauriUpdate | null
	url: string | null
	/** Notes from the update manifest; the dialog prefers GitHub's. */
	notes: string | null
} | null

/**
 * GitHub is polled on a timer rather than on every window focus. A refocus
 * re-check produced a fresh `Update` object each time, which re-opened the
 * banner the user had just dismissed.
 */
const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000

async function checkForUpdate(): Promise<UpdateInfo> {
	// The updater plugin is registered `#[cfg(desktop)]`, so calling it on
	// mobile always rejects. Sideloaded Android asks GitHub directly instead.
	if (IS_MOBILE) {
		if (!IS_ANDROID_UPDATE_CHECK_ENABLED) return null
		const update = await invoke<AndroidUpdate | null>('check_android_update')
		return update
			? {
					version: update.version,
					handle: null,
					url: update.url,
					notes: update.notes || null,
				}
			: null
	}
	// Flatpak builds omit the updater plugin.
	if (IS_WEB || IS_FLATPAK) {
		return null
	}
	// Portable ZIP users must download a new archive; applying the NSIS/MSI
	// updater would install over / beside the extracted folder incorrectly.
	if (await isWindowsPortableBuild()) {
		return null
	}
	const update = await check()
	return update
		? {
				version: update.version,
				handle: update,
				url: null,
				notes: update.body || null,
			}
		: null
}

/**
 * The handle whose package has been downloaded. Held here rather than read
 * back from the query cache: the 6-hourly re-check replaces the cached handle
 * with a fresh one that has no downloaded bytes to install.
 */
let downloadedUpdate: TauriUpdate | null = null

/** Tauri plugin errors arrive as bare strings, not `Error`s. */
function describeError(error: unknown): string {
	if (error instanceof Error) return error.message
	if (typeof error === 'string') return error
	return String(error)
}

export const updaterQueryKeys = {
	all: ['updater'] as const,
	checkUpdate: () => ['updater', 'check'] as const,
}

export const updaterQueryOptions = {
	checkUpdate: () =>
		queryOptions({
			queryKey: updaterQueryKeys.checkUpdate(),
			queryFn: async () => checkForUpdate(),
			retry: 1,
			staleTime: CHECK_INTERVAL_MS,
			refetchInterval: CHECK_INTERVAL_MS,
			refetchOnWindowFocus: false,
			refetchOnReconnect: false,
			// The cached value is the `Update` handle the install path reuses, so
			// it must outlive the component that fetched it.
			gcTime: Number.POSITIVE_INFINITY,
		}),
}

const useCheckUpdateQuery = (
	options?: Omit<
		UseQueryOptions<
			UpdateInfo,
			Error,
			UpdateInfo,
			readonly ['updater', 'check']
		>,
		'queryKey' | 'queryFn'
	>
) => {
	// Deliberately quiet: this is the unattended 6-hourly poll, and a toast for
	// a transient network failure the user never asked about is pure noise. The
	// manual "Check for updates" button reports its own failures.
	// (The `meta.onError` this used to declare was dead code — react-query only
	// calls that through a `QueryCache` handler, which is not configured.)
	return useQuery({
		...updaterQueryOptions.checkUpdate(),
		...options,
	})
}

/**
 * Runs the periodic check and mirrors the outcome into the updater store.
 * Mounted once, by `AppUpdater` — every other surface reads the store.
 *
 * The effect keys off the version *string*, not the `Update` object, so a
 * re-check that finds the same release is a no-op rather than a re-prompt.
 */
export const useUpdaterSync = (enabled: boolean) => {
	const updateFound = useUpdaterStore((s) => s.updateFound)
	const noUpdate = useUpdaterStore((s) => s.noUpdate)
	const markChecked = useUpdaterStore((s) => s.markChecked)
	const query = useCheckUpdateQuery({ enabled })
	const { isSuccess, dataUpdatedAt } = query
	const version = query.data?.version ?? null
	const url = query.data?.url ?? null
	const notes = query.data?.notes ?? null

	useEffect(() => {
		if (!isSuccess) return
		if (version) updateFound(version, url, notes)
		else noUpdate()
	}, [isSuccess, version, url, notes, updateFound, noUpdate])

	// Separate from the effect above: a re-check that finds the same release
	// changes nothing there, but it is still a check.
	useEffect(() => {
		if (isSuccess && dataUpdatedAt > 0) markChecked(dataUpdatedAt)
	}, [isSuccess, dataUpdatedAt, markChecked])

	return query
}

export const useCheckForUpdatesMutation = () => {
	const queryClient = useQueryClient()
	const { t } = useTranslation()
	const updateFound = useUpdaterStore((s) => s.updateFound)
	const noUpdate = useUpdaterStore((s) => s.noUpdate)
	const markChecked = useUpdaterStore((s) => s.markChecked)

	return useMutation({
		mutationFn: async () => checkForUpdate(),
		onSuccess: (update) => {
			// Seed the cache so `install` reuses this handle instead of re-checking.
			queryClient.setQueryData(updaterQueryKeys.checkUpdate(), update)
			if (update) updateFound(update.version, update.url, update.notes)
			else noUpdate()
			markChecked(Date.now())
		},
		onError: (error: Error) => {
			console.error('Failed to check for updates:', error)
			toastManager.add({
				title: t('updater.checkFailed'),
				description: t('updater.checkFailedDesc'),
				type: 'error',
			})
		},
	})
}

/**
 * The one install path, in two steps. `install` only *downloads*: on Windows
 * the updater quits the app as soon as the installer starts, so installing
 * here would kill a running transfer before anyone could be warned. `restart`
 * installs the downloaded package and relaunches, and is what callers gate
 * behind the in-flight-transfer warning.
 */
export const useInstallUpdate = () => {
	const queryClient = useQueryClient()
	const { t } = useTranslation()
	const startDownload = useUpdaterStore((s) => s.startDownload)
	const setContentLength = useUpdaterStore((s) => s.setContentLength)
	const addProgress = useUpdaterStore((s) => s.addProgress)
	const downloadFinished = useUpdaterStore((s) => s.downloadFinished)
	const startInstall = useUpdaterStore((s) => s.startInstall)
	const installFailed = useUpdaterStore((s) => s.installFailed)
	const restarting = useUpdaterStore((s) => s.restarting)
	const fail = useUpdaterStore((s) => s.fail)
	const downloadUrl = useUpdaterStore((s) => s.downloadUrl)

	const install = useCallback(async () => {
		// Sideloaded Android can only point at the release; there is no download
		// to own, and the update stays "available" until the user installs it.
		if (downloadUrl) {
			await openExternalUrl(downloadUrl)
			return
		}
		// Whichever surface clicks first owns the download; the rest no-op.
		if (!startDownload()) return
		try {
			const cached = queryClient.getQueryData<UpdateInfo>(
				updaterQueryKeys.checkUpdate()
			)
			const update = cached ?? (await checkForUpdate())
			if (!update?.handle) {
				fail('The update is no longer available.')
				return
			}
			await update.handle.download((event) => {
				if (event.event === 'Started') {
					if (event.data.contentLength) {
						setContentLength(event.data.contentLength)
					}
				} else if (event.event === 'Progress') {
					addProgress(event.data.chunkLength)
				}
			})
			downloadedUpdate = update.handle
			downloadFinished()
		} catch (error) {
			console.error('Failed to download update:', error)
			fail(describeError(error))
		}
	}, [
		queryClient,
		downloadUrl,
		startDownload,
		setContentLength,
		addProgress,
		downloadFinished,
		fail,
	])

	const restart = useCallback(async () => {
		if (!startInstall()) return
		try {
			if (!downloadedUpdate) {
				// Nothing downloaded to install from; start over.
				fail('The downloaded update was lost. Please try again.')
				return
			}
			// On Windows this call does not return: the installer takes over and
			// relaunches the app itself.
			await downloadedUpdate.install()
		} catch (error) {
			console.error('Failed to install update:', error)
			installFailed(describeError(error))
			return
		}
		restarting()
		try {
			await relaunch()
		} catch (error) {
			console.error('Failed to relaunch after update:', error)
			// Back to `ready` so the spinner doesn't hang; the reason tells the
			// user to quit and reopen, since the install itself went through.
			installFailed(t('updater.restartFailedDesc'))
		}
	}, [startInstall, installFailed, restarting, fail, t])

	return { install, restart }
}

/**
 * Restart-to-update with the in-flight-transfer warning, shared by every
 * surface that offers the restart so none of them can skip it.
 */
export const useRestartToUpdate = () => {
	const { restart } = useInstallUpdate()
	const transferBusy = useTransferBusy()
	const [confirming, setConfirming] = useState(false)

	const requestRestart = useCallback(() => {
		if (transferBusy && !confirming) {
			setConfirming(true)
			return
		}
		setConfirming(false)
		void restart()
	}, [transferBusy, confirming, restart])

	const cancel = useCallback(() => setConfirming(false), [])

	return { confirming, requestRestart, cancel }
}

/** GitHub's notes for one release; cached for the session once fetched. */
export const releaseNotesQueryOptions = (version: string) =>
	queryOptions({
		queryKey: ['updater', 'release-notes', version] as const,
		queryFn: ({ signal }) => fetchReleaseNotes(version, signal),
		staleTime: Number.POSITIVE_INFINITY,
		retry: 1,
		refetchOnWindowFocus: false,
		refetchOnReconnect: false,
	})

const LAST_SEEN_VERSION_KEY = 'dashbeam.updater.last-seen-version'

/**
 * Opens "What's new" once, on the first launch after an update. The notes are
 * fetched first so an offline launch gets no empty popup; the version is
 * recorded either way, and Settings keeps a link to the same dialog.
 */
export const useAnnounceInstalledUpdate = () => {
	const queryClient = useQueryClient()
	const openWhatsNew = useUpdaterStore((s) => s.openWhatsNew)

	useEffect(() => {
		let lastSeen: string | null = null
		try {
			lastSeen = localStorage.getItem(LAST_SEEN_VERSION_KEY)
			if (lastSeen !== VERSION) {
				localStorage.setItem(LAST_SEEN_VERSION_KEY, VERSION)
			}
		} catch {
			// Without storage there is no telling an update from any other launch.
			return
		}
		if (!shouldAnnounceUpdate(lastSeen, VERSION)) return
		queryClient
			.fetchQuery(releaseNotesQueryOptions(VERSION))
			.then(() => openWhatsNew({ version: VERSION, installed: true }))
			.catch((error) => {
				console.warn('Skipping the post-update notes:', error)
			})
	}, [queryClient, openWhatsNew])
}
