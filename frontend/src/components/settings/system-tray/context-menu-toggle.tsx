import { useEffect, useState } from 'react'
import { IS_LINUX } from '@/lib/platform'
import { invoke } from '@/lib/platform-api'
import { useTranslation } from '../../../i18n'
import { useAppSettingStore } from '../../../store/app-setting'
import { FrameDescription, FrameTitle } from '../../ui/frame'
import { Switch } from '../../ui/switch'

async function applyContextMenu(
	enable: boolean,
	allowElevation = true
): Promise<void> {
	await invoke('toggle_context_menu', { enable, allowElevation })
}

/**
 * Keeps the file manager entry (Windows Explorer verb, or the Nautilus script
 * on Linux) in sync with the persisted setting.
 * Runs once after settings hydrate so first launch (default: on) registers
 * without requiring a visit to Settings, and so disable survives restarts.
 * On Linux desktops without Nautilus, enabling is a backend no-op.
 */
export function ContextMenuSync() {
	useEffect(() => {
		let cancelled = false

		const sync = () => {
			if (cancelled) return
			const enabled = useAppSettingStore.getState().windowsContextMenu
			// Quiet sync: never prompt for UAC on launch.
			applyContextMenu(enabled, false).catch((e) =>
				console.error('Failed to sync file manager context menu:', e)
			)
		}

		if (useAppSettingStore.persist.hasHydrated()) {
			sync()
			return () => {
				cancelled = true
			}
		}

		const unsub = useAppSettingStore.persist.onFinishHydration(() => {
			sync()
		})
		return () => {
			cancelled = true
			unsub()
		}
	}, [])

	return null
}

export function ContextMenuToggle() {
	const { t } = useTranslation()
	const enabled = useAppSettingStore((s) => s.windowsContextMenu)
	const setEnabled = useAppSettingStore((s) => s.setWindowsContextMenu)
	const [pending, setPending] = useState(false)
	const [error, setError] = useState<string | null>(null)

	const handleChange = async (value: boolean) => {
		setPending(true)
		setError(null)
		// Optimistic UI; revert if the registry update fails.
		setEnabled(value)
		try {
			await applyContextMenu(value, true)
		} catch (e) {
			setEnabled(!value)
			const message =
				e instanceof Error ? e.message : typeof e === 'string' ? e : String(e)
			setError(message)
			console.error('Failed to toggle file manager context menu:', e)
		} finally {
			setPending(false)
		}
	}

	// Linux has its own strings: the entry lives under Files' Scripts submenu.
	const keyPrefix = IS_LINUX
		? 'settings.general.systembar.contextMenu.linux'
		: 'settings.general.systembar.contextMenu'

	return (
		<div className="flex flex-col gap-1">
			<div className="flex items-center justify-between">
				<div className="flex-1">
					<FrameTitle>{t(`${keyPrefix}.label`)}</FrameTitle>
					<FrameDescription>{t(`${keyPrefix}.description`)}</FrameDescription>
				</div>
				<Switch
					checked={enabled}
					disabled={pending}
					onCheckedChange={handleChange}
				/>
			</div>
			{error ? (
				<p className="text-sm text-destructive" role="alert">
					{error}
				</p>
			) : null}
		</div>
	)
}
