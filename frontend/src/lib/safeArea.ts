import { IS_MOBILE } from './platform'
import { invoke } from './platform-api'

type WindowInsets = {
	top: number
	right: number
	bottom: number
	left: number
}

const EDGES = ['top', 'right', 'bottom', 'left'] as const

/** Cold start can win the race against the first window layout pass. */
const STARTUP_RETRY_DELAYS_MS = [150, 500, 1500]
const RESIZE_DEBOUNCE_MS = 100

function applyInsets(insets: WindowInsets): boolean {
	const root = document.documentElement
	let hasInset = false

	for (const edge of EDGES) {
		const raw = Number(insets?.[edge])
		const value = Number.isFinite(raw) && raw > 0 ? raw : 0
		if (value > 0) hasInset = true
		root.style.setProperty(`--native-inset-${edge}`, `${value}px`)
	}

	return hasInset
}

async function readInsets(): Promise<boolean> {
	try {
		const insets = await invoke<WindowInsets>(
			'plugin:native-utils|get_window_insets'
		)
		return applyInsets(insets)
	} catch (error) {
		console.error('Failed to read window insets:', error)
		return false
	}
}

/**
 * Publishes the native system bar insets as `--native-inset-*` custom properties.
 *
 * Edge-to-edge is enforced from Android API 35, but Android WebView only forwards
 * those insets to `env(safe-area-inset-*)` on some versions, and the iOS webview
 * doesn't report them either, so `--safe-area-*` in index.css takes whichever source
 * reports the larger value.
 */
export function initializeSafeAreaInsets(): void {
	if (!IS_MOBILE) return

	void (async () => {
		if (await readInsets()) return
		for (const delay of STARTUP_RETRY_DELAYS_MS) {
			await new Promise((resolve) => window.setTimeout(resolve, delay))
			if (await readInsets()) return
		}
	})()

	let debounce = 0
	const refresh = () => {
		window.clearTimeout(debounce)
		debounce = window.setTimeout(() => {
			void readInsets()
		}, RESIZE_DEBOUNCE_MS)
	}

	// Rotation, split screen, and (on Android) switching between gesture and
	// three-button navigation all change the insets while the app is running.
	window.addEventListener('resize', refresh)
	window.addEventListener('orientationchange', refresh)
	document.addEventListener('visibilitychange', () => {
		if (document.visibilityState === 'visible') refresh()
	})
}
