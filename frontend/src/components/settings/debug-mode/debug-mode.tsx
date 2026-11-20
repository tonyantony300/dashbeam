import { useState } from 'react'
import { Loader2 } from 'lucide-react'
import { useTranslation } from '../../../i18n'
import {
	DEBUG_LOGGING_SUPPORTED,
	useClearDebugLogs,
	useDebugLogging,
	useExportDebugBundle,
	useSetDebugLogging,
} from '../../../hooks/use-debug-logging'
import {
	downloadDir,
	joinPath,
	revealItemInDir,
	saveDialog,
} from '@/lib/platform-api'
import { IS_ANDROID, IS_IOS } from '@/lib/platform'
import { openDownloadTarget } from '@/plugins/nativeUtils'
import { buildRelayConfigArg } from '@/lib/relay-config'
import { useAppSettingStore } from '../../../store/app-setting'
import { Button } from '../../ui/button'
import {
	Frame,
	FrameDescription,
	FrameFooter,
	FramePanel,
	FrameTitle,
} from '../../ui/frame'
import { Switch } from '../../ui/switch'
import { toastManager } from '../../ui/toast'

/** Longer than the 3s global default: these messages ask the user to do something. */
const TOAST_TIMEOUT_MS = 5000

/**
 * Base UI pauses auto-dismiss while the window is blurred, so opening Finder or
 * a save dialog can leave a toast stuck — force-close after the timeout.
 */
function showToast(options: Parameters<typeof toastManager.add>[0]) {
	const id = toastManager.add({ ...options, timeout: TOAST_TIMEOUT_MS })
	window.setTimeout(() => toastManager.close(id), TOAST_TIMEOUT_MS)
	return id
}

function defaultBundleName() {
	const stamp = new Date().toISOString().slice(0, 19).replace(/[:T]/g, '-')
	return `dashbeam-diagnostics-${stamp}.txt`
}

export function DebugMode() {
	const { t } = useTranslation()
	const { data: state } = useDebugLogging()
	const setDebugLogging = useSetDebugLogging()
	const exportBundle = useExportDebugBundle()
	const clearLogs = useClearDebugLogs()
	const [error, setError] = useState<string | null>(null)
	const relayMode = useAppSettingStore((s) => s.relayMode)
	const relayUrls = useAppSettingStore((s) => s.relayUrls)
	const relayAuthToken = useAppSettingStore((s) => s.relayAuthToken)
	const relayFallback = useAppSettingStore((s) => s.relayFallback)

	if (!DEBUG_LOGGING_SUPPORTED || !state) return null

	const { enabled, activeThisSession } = state
	// Verbosity is decided at startup, so a toggle only lands after a relaunch.
	const restartRequired = enabled !== activeThisSession

	const handleToggle = async (value: boolean) => {
		setError(null)
		try {
			await setDebugLogging.mutateAsync(value)
			showToast({
				title: t('settings.general.debugMode.restartTitle'),
				description: t('settings.general.debugMode.restartDescription'),
				type: 'info',
			})
		} catch (e) {
			setError(e instanceof Error ? e.message : String(e))
		}
	}

	const handleSave = async () => {
		setError(null)
		try {
			// iOS has no save-as picker: the dialog plugin exports a placeholder and
			// returns a `file://` URL that may not be writable. Save into Documents
			// (visible in the Files app) instead; the Quick Look preview opened
			// below can share it onward.
			const destPath = IS_IOS
				? await joinPath(await downloadDir(), defaultBundleName())
				: await saveDialog({
						defaultPath: defaultBundleName(),
						filters: [{ name: 'Text', extensions: ['txt'] }],
					})
			if (!destPath) return

			await exportBundle.mutateAsync({
				destPath,
				relay: buildRelayConfigArg({
					relayMode,
					relayUrls,
					relayAuthToken,
					relayFallback,
				}),
			})
			showToast({
				title: t('settings.general.debugMode.savedTitle'),
				description: t('settings.general.debugMode.savedDescription'),
				type: 'success',
			})
			// Best-effort: the bundle is already written, so failing to show it
			// must not report the save as failed.
			try {
				// `destPath` is a SAF `content://` URI on Android, which the
				// opener plugin cannot reveal — it has no Android support at all,
				// and can't reveal on iOS either.
				if (IS_ANDROID || IS_IOS) await openDownloadTarget(destPath)
				else await revealItemInDir(destPath)
			} catch (revealError) {
				console.warn('Could not show the saved diagnostics:', revealError)
			}
		} catch (e) {
			setError(e instanceof Error ? e.message : String(e))
		}
	}

	const handleClear = async () => {
		setError(null)
		try {
			await clearLogs.mutateAsync()
			showToast({
				title: t('settings.general.debugMode.clearedTitle'),
				type: 'success',
			})
		} catch (e) {
			setError(e instanceof Error ? e.message : String(e))
		}
	}

	return (
		<Frame>
			<FramePanel className="flex items-center justify-between">
				<div className="flex-1">
					<FrameTitle>{t('settings.general.debugMode.label')}</FrameTitle>
					<FrameDescription>
						{t('settings.general.debugMode.description')}
					</FrameDescription>
					{restartRequired ? (
						<p className="mt-1 text-sm text-muted-foreground">
							{enabled
								? t('settings.general.debugMode.pendingOn')
								: t('settings.general.debugMode.pendingOff')}
						</p>
					) : null}
					{error ? (
						<p className="mt-1 text-sm text-destructive" role="alert">
							{error}
						</p>
					) : null}
				</div>
				<Switch
					checked={enabled}
					disabled={setDebugLogging.isPending}
					onCheckedChange={handleToggle}
				/>
			</FramePanel>
			{activeThisSession ? (
				<FrameFooter className="flex-row justify-end gap-2">
					<Button
						variant="ghost"
						onClick={handleClear}
						disabled={clearLogs.isPending}
					>
						{t('settings.general.debugMode.clear')}
					</Button>
					<Button
						variant="secondary"
						onClick={handleSave}
						disabled={exportBundle.isPending}
					>
						{exportBundle.isPending ? (
							<Loader2 className="mr-2 h-4 w-4 animate-spin" />
						) : null}
						{t('settings.general.debugMode.save')}
					</Button>
				</FrameFooter>
			) : null}
		</Frame>
	)
}
