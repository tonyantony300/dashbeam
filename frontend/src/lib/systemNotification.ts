import {
	isPermissionGranted,
	requestPermission,
	sendNotification,
	type Options as NotificationOptions,
} from '@tauri-apps/plugin-notification'
import { resolveResource } from '@tauri-apps/api/path'
import { IS_TAURI, IS_WINDOWS, IS_MOBILE, IS_DESKTOP } from './platform'
import { invoke } from './platform-api'
import { useAppSettingStore } from '../store/app-setting'
import { shouldNotify } from './notification-gate'

type SystemNotificationOptions = Pick<NotificationOptions, 'title' | 'body'>

const NOTIFICATION_ICON_RESOURCE = 'icons/128x128.png'
let cachedNotificationIconPath: string | null | undefined

async function getNotificationIconPath(): Promise<string | undefined> {
	if (!IS_TAURI || IS_WINDOWS) {
		return undefined
	}

	if (cachedNotificationIconPath !== undefined) {
		return cachedNotificationIconPath ?? undefined
	}

	try {
		const iconPath = await resolveResource(NOTIFICATION_ICON_RESOURCE)
		cachedNotificationIconPath = iconPath
		return iconPath
	} catch (error) {
		console.warn('Failed to resolve notification icon resource:', error)
		cachedNotificationIconPath = null
		return undefined
	}
}

/**
 * Whether the user is looking at the app. Desktop reads the real window state;
 * mobile uses page visibility, since the webview is the app and Android's focus
 * APIs are unreliable.
 */
async function isAppInForeground(): Promise<boolean> {
	if (IS_MOBILE) {
		return (
			typeof document !== 'undefined' && document.visibilityState === 'visible'
		)
	}
	const { getCurrentWindow } = await import('@tauri-apps/api/window')
	const window = getCurrentWindow()
	const [focused, visible] = await Promise.all([
		window.isFocused(),
		window.isVisible(),
	])
	return focused && visible
}

let permissionBootstrapped = false

/**
 * Ask for notification permission once, while the app is on screen.
 *
 * `sendSystemNotification` can't do this itself — it only fires when the app is
 * backgrounded, and Android's `POST_NOTIFICATIONS` dialog can't be shown by a
 * non-resumed Activity. Call from a mounted component; safe to call repeatedly.
 */
export async function ensureNotificationPermission(): Promise<boolean> {
	if (!IS_TAURI || permissionBootstrapped) {
		return false
	}
	permissionBootstrapped = true

	try {
		if (await isPermissionGranted()) {
			return true
		}
		return (await requestPermission()) === 'granted'
	} catch (error) {
		console.warn('Failed to bootstrap notification permission:', error)
		return false
	}
}

export async function sendSystemNotification(
	options: SystemNotificationOptions
): Promise<boolean> {
	if (!IS_TAURI) {
		return false
	}

	try {
		// Fail open: a redundant notification beats a missed one.
		let foreground = false
		try {
			foreground = await isAppInForeground()
		} catch (error) {
			console.warn('Failed to read window foreground state:', error)
		}

		if (
			!shouldNotify({
				enabled: useAppSettingStore.getState().enableNotifications,
				foreground,
			})
		) {
			return false
		}

		// Fallback if `ensureNotificationPermission` never ran — see its doc for
		// why requesting from here is a bad moment on Android.
		let granted = await isPermissionGranted()
		if (!granted) {
			const permission = await requestPermission()
			granted = permission === 'granted'
		}

		if (!granted) {
			return false
		}

		const icon = await getNotificationIconPath()
		// Desktop goes through our command for `Timeout::Never`; the plugin would
		// use the ~2s OS default. Android keeps the plugin path.
		if (IS_DESKTOP) {
			await invoke('show_system_notification', {
				title: options.title,
				body: options.body,
				icon,
			})
		} else {
			sendNotification(icon ? { ...options, icon } : options)
		}
		return true
	} catch (error) {
		console.error('Failed to send system notification:', error)
		return false
	}
}
