export type { UnlistenFn } from '@tauri-apps/api/event'

import type { UnlistenFn } from '@tauri-apps/api/event'
import { IS_IOS, IS_TAURI, IS_WEB } from './platform'
import type { RelayConfigArg } from './relay-config'
import {
	ensureWasmBridge,
	getWebSharingTicket,
	wasmFetchTicketMetadata,
	wasmGetRelayStatus,
	wasmReceiveFile,
	wasmSendItems,
	wasmStopSharing,
	wasmVerifyRelays,
} from './wasm-bridge-client'
import { dispatchWebEvent, subscribeWebEvent } from './web-event-bus'
import {
	getWebFile,
	isWebDirectory,
	listWebFileEntriesUnderPath,
	registerWebDirectory,
	registerWebFile,
	webFilePathKey,
} from './web-file-store'
import { WebPreviewError } from './web-preview-error'
import {
	initWebSaveLocation,
	writeReceivedCollection,
} from './web-save-location'
import { collectWebSendPayload } from './web-send-items'

type DialogOptions = {
	multiple?: boolean
	directory?: boolean
}

type TauriWindowStub = {
	minimize: () => Promise<void>
	toggleMaximize: () => Promise<void>
	close: () => Promise<void>
	listen: <T>(
		event: string,
		handler: (event: { payload: T }) => void
	) => Promise<UnlistenFn>
}

const noopUnlisten: UnlistenFn = () => {}

const webWindowStub: TauriWindowStub = {
	minimize: async () => {},
	toggleMaximize: async () => {},
	close: async () => {},
	listen: async () => noopUnlisten,
}

function webTransferUnavailable(): never {
	throw new WebPreviewError(
		'Web transfers require a built WASM engine. Run: pnpm build:wasm'
	)
}

function relayFromArgs(
	args?: Record<string, unknown>
): RelayConfigArg | undefined {
	const relay = args?.relay
	if (relay && typeof relay === 'object') {
		return relay as RelayConfigArg
	}
	return undefined
}

async function invokeWebTransfer<T>(
	cmd: string,
	args?: Record<string, unknown>
): Promise<T> {
	switch (cmd) {
		case 'fetch_ticket_metadata': {
			const ticket = String(args?.ticket ?? '').trim()
			if (!ticket) {
				throw new Error('Ticket is required')
			}
			const json = await wasmFetchTicketMetadata(ticket, relayFromArgs(args))
			return JSON.parse(json) as T
		}
		case 'send_items':
		case 'start_sharing': {
			const paths =
				cmd === 'start_sharing'
					? [String(args?.path ?? '')]
					: ((args?.paths as string[] | undefined) ?? [])

			if (!paths.length || paths.some((path) => !path)) {
				throw new Error('No file selected')
			}

			const payload = await collectWebSendPayload(paths)
			const ticket = await wasmSendItems(
				payload.names,
				payload.bytesList,
				payload.entryType,
				payload.metadataJson,
				relayFromArgs(args)
			)
			return ticket as T
		}
		case 'receive_file': {
			const ticket = String(args?.ticket ?? '').trim()
			if (!ticket) {
				throw new Error('Ticket is required')
			}

			const { fileNames, bytesList } = await wasmReceiveFile(
				ticket,
				relayFromArgs(args)
			)

			if (fileNames.length !== bytesList.length) {
				throw new Error('Received file payload was incomplete')
			}

			const files = fileNames.map((name, index) => ({
				name,
				bytes: bytesList[index]!,
			}))

			const writeResult = await writeReceivedCollection(files)

			if (writeResult.conflicts.length > 0) {
				dispatchWebEvent(
					'receive-conflicts',
					JSON.stringify(writeResult.conflicts)
				)
			}

			dispatchWebEvent('receive-completed')

			if (files.length === 1) {
				return `Downloaded ${files[0]!.name}` as T
			}

			if (writeResult.zippedDownload) {
				return `Downloaded ${files.length} files as a ZIP archive` as T
			}

			return `Downloaded ${files.length} files` as T
		}
		case 'stop_sharing':
			await wasmStopSharing()
			return undefined as T
		case 'verify_relays': {
			const relay = relayFromArgs(args)
			if (!relay) {
				throw new Error('Relay config is required')
			}
			return (await wasmVerifyRelays(relay)) as T
		}
		case 'get_relay_status':
			return (await wasmGetRelayStatus(relayFromArgs(args))) as T
		default:
			return invokeWebStub<T>(cmd, args)
	}
}

async function invokeWeb<T>(
	cmd: string,
	args?: Record<string, unknown>
): Promise<T> {
	switch (cmd) {
		case 'fetch_ticket_metadata':
		case 'send_items':
		case 'start_sharing':
		case 'receive_file':
		case 'stop_sharing':
		case 'verify_relays':
		case 'get_relay_status':
			try {
				await ensureWasmBridge()
			} catch (error) {
				console.error('Failed to initialize wasm-bridge:', error)
				webTransferUnavailable()
			}
			return invokeWebTransfer<T>(cmd, args)
		default:
			return invokeWebStub<T>(cmd, args)
	}
}

function invokeWebStub<T>(cmd: string, args?: Record<string, unknown>): T {
	switch (cmd) {
		case 'check_launch_intent':
			return null as T
		case 'check_path_type': {
			const path = String(args?.path ?? '')
			if (isWebDirectory(path)) {
				return 'directory' as T
			}
			if (getWebFile(path)) {
				return 'file' as T
			}
			return (path.endsWith('/') ? 'directory' : 'file') as T
		}
		case 'get_paths_mime_types': {
			const paths = (args?.paths as string[] | undefined) ?? []
			return paths.map((path) => {
				if (isWebDirectory(path)) {
					return 'inode/directory'
				}
				const file = getWebFile(path)
				return file?.type || null
			}) as T
		}
		case 'get_file_size': {
			const path = String(args?.path ?? '')
			const file = getWebFile(path)
			if (file) {
				return file.size as T
			}
			if (isWebDirectory(path)) {
				return listWebFileEntriesUnderPath(path).reduce(
					(total, entry) => total + entry.file.size,
					0
				) as T
			}
			return 0 as T
		}
		case 'get_sharing_status':
			return getWebSharingTicket() as T
		case 'get_node_status':
			return {
				status: 'unavailable',
				reason: 'desktop_only',
				network_ready: false,
			} as T
		case 'get_device_info':
			return null as T
		case 'set_device_display_name':
			return null as T
		case 'get_pairing_ticket':
			return null as T
		case 'list_paired_devices':
			return [] as T
		// Transfer history is desktop/Android only: the browser receive path
		// writes through a directory handle or a zip download, so "where it
		// saved" has no stable meaning to record.
		case 'list_transfer_history':
			return [] as T
		case 'get_transfer_temp_data':
			return { exists: false, sizeBytes: 0 } as T
		// Web keeps no history, so no row can have a target to open.
		case 'transfer_open_target':
			return null as T
		case 'delete_transfer_record':
		case 'clear_transfer_history':
		case 'clear_transfer_temp_data':
			return undefined as T
		case 'start_pairing_host':
		case 'join_pairing':
		case 'forget_paired_device':
		case 'rename_paired_device':
		case 'trust_paired_device':
		case 'invite_paired_device':
		case 'stop_pairing_host':
		case 'reconfigure_node_relay':
			return undefined as T
		case 'list_nearby':
			// Browsers have no multicast; Nearby is native-only.
			return [] as unknown as T
		case 'nearby_status':
			return { reason: null } as unknown as T
		case 'get_discoverability':
			return 'off' as unknown as T
		case 'set_discoverability':
			return undefined as unknown as T
		case 'invite_nearby_device':
			throw new WebPreviewError('invite_nearby_device')
		case 'request_nearby_pair':
			throw new WebPreviewError('request_nearby_pair')
		case 'respond_nearby_invite':
			throw new WebPreviewError('respond_nearby_invite')
		case 'toggle_context_menu':
		case 'plugin:native-utils|select_send_document':
		case 'plugin:native-utils|select_send_folder':
		case 'plugin:native-utils|consume_share_intent':
		case 'plugin:native-utils|cancel_job':
		case 'plugin:native-utils|open_download_folder':
			return null as T
		case 'is_windows_portable':
			return false as T
		case 'check_android_update':
			return null as T
		case 'autostart_is_enabled':
			return null as T
		case 'autostart_set':
			return false as T
		case 'set_background_on_close':
		case 'set_tray_labels':
		case 'show_system_notification':
			return undefined as unknown as T
		default:
			console.warn(`[web] unhandled invoke: ${cmd}`)
			throw new WebPreviewError()
	}
}

export async function invoke<T>(
	cmd: string,
	args?: Record<string, unknown>
): Promise<T> {
	if (IS_WEB) {
		return invokeWeb<T>(cmd, args)
	}

	const { invoke: tauriInvoke } = await import('@tauri-apps/api/core')
	return tauriInvoke<T>(cmd, args)
}

export async function listen<T>(
	event: string,
	handler: (event: { payload: T }) => void
): Promise<UnlistenFn> {
	if (IS_WEB) {
		return subscribeWebEvent(
			event,
			handler as (event: { payload: unknown }) => void
		)
	}

	const { listen: tauriListen } = await import('@tauri-apps/api/event')
	return tauriListen<T>(event, handler)
}

export async function listenForReceiveLinks(
	handler: (url: string) => void
): Promise<UnlistenFn> {
	handler(window.location.href)
	if (IS_WEB) return noopUnlisten

	try {
		const { getCurrent, onOpenUrl } = await import(
			'@tauri-apps/plugin-deep-link'
		)
		const unlisten = await onOpenUrl((urls) => urls.forEach(handler))
		try {
			const current = await getCurrent()
			current?.forEach(handler)
		} catch (error) {
			console.error('Failed to read the current receive link:', error)
		}
		return unlisten
	} catch (error) {
		console.error('Failed to listen for receive links:', error)
		return noopUnlisten
	}
}

export async function openDialog(
	options: DialogOptions = {}
): Promise<string | string[] | null> {
	if (IS_TAURI) {
		const { open } = await import('@tauri-apps/plugin-dialog')
		return open(options)
	}

	return pickPathsInBrowser(options)
}

function pickPathsInBrowser(
	options: DialogOptions
): Promise<string | string[] | null> {
	return new Promise((resolve) => {
		const input = document.createElement('input')
		input.type = 'file'
		input.style.display = 'none'
		input.multiple = options.multiple ?? false

		if (options.directory) {
			input.setAttribute('webkitdirectory', '')
			input.setAttribute('directory', '')
		}

		const cleanup = () => {
			input.remove()
		}

		input.addEventListener('change', () => {
			const files = input.files
			cleanup()

			if (!files?.length) {
				resolve(null)
				return
			}

			if (options.directory) {
				const topLevel = new Set<string>()
				for (const file of Array.from(files)) {
					const path = webFilePathKey(file)
					registerWebFile(path, file)
					const [root] = path.split('/')
					if (root) topLevel.add(root)
				}
				const folders = [...topLevel]
				for (const folder of folders) {
					registerWebDirectory(folder)
				}
				resolve(options.multiple ? folders : (folders[0] ?? null))
				return
			}

			const paths: string[] = []
			for (const file of Array.from(files)) {
				const path = webFilePathKey(file)
				registerWebFile(path, file)
				paths.push(path)
			}
			resolve(options.multiple ? paths : (paths[0] ?? null))
		})

		input.addEventListener('cancel', () => {
			cleanup()
			resolve(null)
		})

		document.body.appendChild(input)
		input.click()
	})
}

/**
 * Native "save as" picker. Web has no equivalent that returns a writable path, and the
 * only caller (debug mode) is Tauri-gated, so it resolves to null there.
 */
export async function saveDialog(options: {
	defaultPath?: string
	filters?: { name: string; extensions: string[] }[]
}): Promise<string | null> {
	if (IS_TAURI) {
		const { save } = await import('@tauri-apps/plugin-dialog')
		return save(options)
	}

	return null
}

export async function downloadDir(): Promise<string> {
	// iOS has no shared Downloads folder; the app's Documents folder is what
	// the Files app shows under "On My iPhone".
	if (IS_IOS) {
		const { documentDir } = await import('@tauri-apps/api/path')
		return documentDir()
	}
	if (IS_TAURI) {
		const { downloadDir: tauriDownloadDir } = await import(
			'@tauri-apps/api/path'
		)
		return tauriDownloadDir()
	}

	const saved = await initWebSaveLocation()
	return saved || 'Browser downloads'
}

export {
	pickDownloadDirectory,
	supportsWebSaveLocationPicker,
} from './web-save-location'

export async function joinPath(...paths: string[]): Promise<string> {
	if (IS_TAURI) {
		const { join } = await import('@tauri-apps/api/path')
		return join(...paths)
	}

	return paths.filter(Boolean).join('/')
}

export async function revealItemInDir(_path: string): Promise<void> {
	if (IS_TAURI) {
		const { revealItemInDir: tauriReveal } = await import(
			'@tauri-apps/plugin-opener'
		)
		await tauriReveal(_path)
	}
}

export async function getCurrentWindow(): Promise<TauriWindowStub> {
	if (IS_TAURI) {
		const { getCurrentWindow: tauriGetCurrentWindow } = await import(
			'@tauri-apps/api/window'
		)
		return tauriGetCurrentWindow()
	}

	return webWindowStub
}
