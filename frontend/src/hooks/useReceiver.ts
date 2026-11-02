import { useCallback, useEffect, useRef, useState } from 'react'
import type { PairedInvitePayload } from '@/lib/pairing-api'
import { IS_ANDROID, IS_IOS, IS_WEB } from '@/lib/platform'
import {
	downloadDir,
	invoke,
	joinPath,
	listen,
	openDialog,
	pickDownloadDirectory,
	revealItemInDir,
	supportsWebSaveLocationPicker,
	type UnlistenFn,
} from '@/lib/platform-api'
import {
	getWebPreviewErrorMessage,
	isWebPreviewError,
} from '@/lib/web-preview-error'
import {
	openDownloadFolder,
	openDownloadTarget,
	selectDownloadFolder,
} from '@/plugins/nativeUtils'
import { useAppSettingStore } from '@/store/app-setting'
import { isReceiveSessionBusy } from '@/lib/receive-session'
import {
	useReceiverActionsStore,
	type AcceptPairedInviteOptions,
} from '@/store/receiver-actions-store'
import { useTransferTabStore } from '@/store/transfer-tab-store'
import { useTranslation } from '../i18n/react-i18next-compat'
import { getRelayConfigArg } from '../lib/relay'
import { getDiscoveryConfigArg } from '../lib/discovery'
import { ticketFromReceiveLink } from '../lib/receive-link'
import { sendSystemNotification } from '../lib/systemNotification'
import type {
	TicketPreviewMetadata,
	TransferMetadata,
	TransferProgress,
} from '../types/transfer'
import type { AlertDialogState, AlertType } from '../types/ui'
import {
	parseCompletionPayload,
	parseProgressPayload,
} from '../lib/transfer-events'

interface BackendFileMetadata {
	file_name: string
	item_count: number
	size: number
	thumbnail?: string | null
	mime_type?: string | null
	items?:
		| {
				file_name: string
				size: number
				thumbnail?: string | null
				mime_type?: string | null
		  }[]
		| null
}

const isAbsolutePath = (path: string) => {
	if (!path) return false
	return path.startsWith('/') || /^[A-Za-z]:[\\/]/.test(path)
}

const normalizeSeparators = (path: string) => path.replace(/\\/g, '/')

const countTopLevelItems = (names: string[]) => {
	const topLevelItems = new Set<string>()

	for (const name of names) {
		const normalized = normalizeSeparators(name)
		if (!normalized) continue

		if (isAbsolutePath(normalized)) {
			const segments = normalized.split('/').filter(Boolean)
			const lastSegment = segments[segments.length - 1]
			if (lastSegment) {
				topLevelItems.add(lastSegment)
			}
			continue
		}

		const [topLevel] = normalized.split('/')
		if (topLevel) {
			topLevelItems.add(topLevel)
		}
	}

	return topLevelItems.size
}

export interface UseReceiverReturn {
	ticket: string
	isReceiving: boolean
	isTransporting: boolean
	isCompleted: boolean
	savePath: string
	alertDialog: AlertDialogState
	transferMetadata: TransferMetadata | null
	transferProgress: TransferProgress | null
	previewMetadata: TicketPreviewMetadata | null
	isPreviewLoading: boolean
	/** Android is still copying the receive out of app-private staging. */
	isExportPending: boolean
	fileNames: string[]

	handleTicketChange: (ticket: string) => void
	handleBrowseFolder: () => Promise<void>
	handleReceive: () => Promise<void>
	handleOpenFolder: () => Promise<void>
	showAlert: (title: string, description: string, type?: AlertType) => void
	closeAlert: () => void
	resetForNewTransfer: () => Promise<void>
}

export function useReceiver(): UseReceiverReturn {
	const { t } = useTranslation()
	const [ticket, setTicket] = useState('')
	const [isReceiving, setIsReceiving] = useState(false)
	const [isTransporting, setIsTransporting] = useState(false)
	const [isCompleted, setIsCompleted] = useState(false)
	/**
	 * Android is still copying the received files out of app-private staging.
	 * The success screen goes up on `receive-completed`, which fires before that
	 * copy starts, so "Open" has nowhere to point until the export finishes.
	 */
	const [isExportPending, setIsExportPending] = useState(false)
	const [savePath, setSavePath] = useState('')
	const downloadsPath = useAppSettingStore((state) => state.downloadsPath)
	const setDownloadsPath = useAppSettingStore((state) => state.setDownloadsPath)
	const downloadsUri = useAppSettingStore((state) => state.downloadsUri)
	const setDownloadsUri = useAppSettingStore((state) => state.setDownloadsUri)
	const downloadsUriRef = useRef(downloadsUri)
	const downloadsPathRef = useRef(downloadsPath)
	const [transferMetadata, setTransferMetadata] =
		useState<TransferMetadata | null>(null)
	const [transferProgress, setTransferProgress] =
		useState<TransferProgress | null>(null)
	const [transferStartTime, setTransferStartTime] = useState<number | null>(
		null
	)
	const [fileNames, setFileNames] = useState<string[]>([])
	const [previewMetadata, setPreviewMetadata] =
		useState<TicketPreviewMetadata | null>(null)
	const [isPreviewLoading, setIsPreviewLoading] = useState(false)
	const [alertDialog, setAlertDialog] = useState<AlertDialogState>({
		isOpen: false,
		title: '',
		description: '',
		type: 'info',
	})
	const pendingConflictNoticeRef = useRef<string | null>(null)
	/**
	 * `content://` URIs a MediaStore export just published. There's no tree URI,
	 * so these are what "Open" works with — one file opens directly, several
	 * open the folder below.
	 */
	const androidMediaStoreUrisRef = useRef<string[]>([])
	/** Where that export landed, relative to storage: `Download/DashBeam`. */
	const androidMediaStorePathRef = useRef('')

	const fileNamesRef = useRef<string[]>([])
	const transferProgressRef = useRef<TransferProgress | null>(null)
	const transferStartTimeRef = useRef<number | null>(null)
	const savePathRef = useRef<string>('')
	const folderOpenTriggeredRef = useRef(false)
	// SAF tree URI that can be opened after a successful Android export.
	// Cleared when receive falls back to the app-private staging folder.
	const androidOpenUriRef = useRef('')
	const previewRequestSeqRef = useRef(0)
	const previewMetadataRef = useRef<TicketPreviewMetadata | null>(null)
	const transferItemCountRef = useRef<number | undefined>(undefined)
	// Incremented each time a new transfer starts or is cancelled. Event listeners
	// capture this value and ignore events whose seq no longer matches — preventing
	// ghost completions from a just-cancelled download.
	const transferSeqRef = useRef(0)
	/**
	 * Where the last completed receive actually wrote. Differs from `savePath`
	 * when an auto-accepted transfer filed itself under a per-device subfolder,
	 * so "Open" must prefer it.
	 */
	const completedOutputDirRef = useRef<string>('')

	const resolveRevealPath = async (basePath: string, names: string[]) => {
		if (!basePath) return null

		if (names.length === 0) {
			return basePath
		}

		if (names.length === 1) {
			const [name] = names
			if (isAbsolutePath(name)) {
				return name
			}
			try {
				return await joinPath(basePath, name)
			} catch (error) {
				console.error('Failed to join path for reveal:', error)
				return basePath
			}
		}

		const firstName = names[0]

		if (isAbsolutePath(firstName)) {
			const normalized = normalizeSeparators(firstName)
			const parts = normalized.split('/')
			if (parts.length > 1) {
				parts.pop()
				return parts.join('/') || firstName
			}
			return firstName
		}

		const normalized = normalizeSeparators(firstName)
		const [topLevel] = normalized.split('/')
		if (topLevel) {
			try {
				return await joinPath(basePath, topLevel)
			} catch (error) {
				console.error('Failed to join directory path for reveal:', error)
			}
		}

		return basePath
	}

	useEffect(() => {
		fileNamesRef.current = fileNames
	}, [fileNames])

	useEffect(() => {
		transferProgressRef.current = transferProgress
	}, [transferProgress])

	useEffect(() => {
		transferStartTimeRef.current = transferStartTime
	}, [transferStartTime])

	useEffect(() => {
		savePathRef.current = savePath
	}, [savePath])

	useEffect(() => {
		downloadsUriRef.current = downloadsUri
	}, [downloadsUri])

	useEffect(() => {
		downloadsPathRef.current = downloadsPath
	}, [downloadsPath])

	useEffect(() => {
		const seq = ++previewRequestSeqRef.current

		if (isReceiving) {
			setIsPreviewLoading(false)
			return
		}

		const trimmed = ticket.trim()
		if (!trimmed) {
			setPreviewMetadata(null)
			previewMetadataRef.current = null
			setIsPreviewLoading(false)
			return
		}

		setIsPreviewLoading(true)
		// Clear stale preview while typing/fetching
		setPreviewMetadata(null)
		previewMetadataRef.current = null

		const timer = window.setTimeout(async () => {
			try {
				const payload = await invoke<BackendFileMetadata>(
					'fetch_ticket_metadata',
					{
						ticket: trimmed,
						relay: getRelayConfigArg(),
						discovery: getDiscoveryConfigArg(),
					}
				)

				if (previewRequestSeqRef.current !== seq) {
					return
				}

				const metadata = {
					fileName: payload.file_name,
					itemCount: payload.item_count,
					size: payload.size,
					thumbnail: payload.thumbnail ?? undefined,
					mimeType: payload.mime_type ?? undefined,
					items: payload.items?.map((item) => ({
						fileName: item.file_name,
						size: item.size,
						thumbnail: item.thumbnail ?? undefined,
						mimeType: item.mime_type ?? undefined,
					})),
				}
				setPreviewMetadata(metadata)
				previewMetadataRef.current = metadata
			} catch (error) {
				if (previewRequestSeqRef.current !== seq) {
					return
				}
				console.warn('Failed to fetch ticket preview metadata:', error)
				setPreviewMetadata(null)
				previewMetadataRef.current = null
			} finally {
				if (previewRequestSeqRef.current === seq) {
					setIsPreviewLoading(false)
				}
			}
		}, 300)

		return () => {
			window.clearTimeout(timer)
		}
	}, [ticket, isReceiving])

	const showAlert = useCallback(
		(title: string, description: string, type: AlertType = 'info') => {
			setAlertDialog({ isOpen: true, title, description, type })
		},
		[]
	)

	const closeAlert = useCallback(() => {
		setAlertDialog((prev) => ({ ...prev, isOpen: false }))
	}, [])

	useEffect(() => {
		const initializeSavePath = async () => {
			try {
				if (IS_ANDROID) {
					setSavePath(downloadsPath)
				} else {
					const downloadsPath = await downloadDir()
					setSavePath(downloadsPath)
				}
			} catch (error) {
				console.error('Failed to get downloads directory:', error)
				setSavePath('')
			}
		}
		initializeSavePath()
	}, [downloadsPath])

	useEffect(() => {
		let disposed = false
		const unlistenFns: UnlistenFn[] = []

		const registerListener = async (
			eventName: string,
			handler: Parameters<typeof listen>[1]
		) => {
			const unlisten = await listen(eventName, handler)
			if (disposed) {
				unlisten()
				return
			}
			unlistenFns.push(unlisten)
		}

		const setupListeners = async () => {
			await registerListener('receive-started', () => {
				if (transferSeqRef.current === 0) return
				setIsTransporting(true)
				setIsCompleted(false)
				setTransferStartTime(Date.now())
				setTransferProgress(null)
			})

			await registerListener('receive-progress', (event: any) => {
				if (transferSeqRef.current === 0) return
				try {
					// The engine already windows the speed and derives the ETA
					// from it; averaging again here only adds lag.
					const progress = parseProgressPayload(event.payload as string)
					if (progress) {
						setTransferProgress(progress)
					}
				} catch (error) {
					console.error('Failed to parse progress event:', error)
				}
			})

			await registerListener('receive-file-names', (event: any) => {
				if (transferSeqRef.current === 0) return
				try {
					const payload = event.payload as string
					const names = JSON.parse(payload) as string[]

					setFileNames(names)
					fileNamesRef.current = names
				} catch (error) {
					console.error('Failed to parse file names event:', error)
				}
			})

			await registerListener('receive-download-fallback', (event: any) => {
				if (!IS_ANDROID) return
				try {
					const payload = event.payload as
						| string
						| { path?: string; reason?: string }
					const fallbackPath =
						typeof payload === 'string'
							? payload.trim()
							: String(payload?.path ?? '').trim()
					const reason =
						typeof payload === 'object' ? payload?.reason : undefined
					if (!fallbackPath) return
					androidOpenUriRef.current = ''
					setSavePath(fallbackPath)
					setTransferMetadata((prev) =>
						prev ? { ...prev, downloadPath: fallbackPath } : prev
					)
					showAlert(
						t('common:receiver.downloadFallbackTitle'),
						reason === 'saf'
							? t('common:receiver.downloadFallbackSafDescription', {
									path: fallbackPath,
								})
							: t('common:receiver.downloadFallbackDescription', {
									path: fallbackPath,
								}),
						'info'
					)
				} catch (error) {
					console.error('Failed to handle download fallback notice:', error)
				}
			})

			// Files reached the public Downloads collection; the success screen
			// shows the path and the URIs make "Open" work without a tree URI.
			await registerListener('receive-download-mediastore', (event: any) => {
				if (!IS_ANDROID) return
				try {
					const payload = event.payload as {
						path?: string
						uris?: string[]
					}
					const path = String(payload?.path ?? '').trim()
					androidMediaStoreUrisRef.current = Array.isArray(payload?.uris)
						? payload.uris
						: []
					androidMediaStorePathRef.current = path
					if (!path) return
					setSavePath(path)
					setTransferMetadata((prev) =>
						prev ? { ...prev, downloadPath: path } : prev
					)
				} catch (error) {
					console.error('Failed to handle MediaStore export notice:', error)
				}
			})

			// The export is done, so "Open" now has somewhere to go.
			await registerListener('receive-export-finished', () => {
				setIsExportPending(false)
			})

			await registerListener('receive-conflicts', (event: any) => {
				if (transferSeqRef.current === 0) return
				try {
					const payload = event.payload as string
					const conflicts = JSON.parse(payload) as Array<{
						original: string
						resolved: string
					}>

					if (conflicts.length === 0) return

					const basename = (p: string) =>
						normalizeSeparators(p).split('/').pop() || p
					const preview = conflicts
						.slice(0, 3)
						.map((c) => `${basename(c.original)} → ${basename(c.resolved)}`)
						.join('\n')

					pendingConflictNoticeRef.current =
						conflicts.length > 3
							? `${preview}\n${t('common:receiver.conflictsMore', {
									count: conflicts.length - 3,
								})}`
							: preview
				} catch (error) {
					console.error('Failed to parse receive-conflicts event:', error)
				}
			})

			await registerListener('receive-completed', (event: any) => {
				if (transferSeqRef.current === 0) return
				setIsTransporting(false)
				setIsCompleted(true)
				setTransferProgress(null)

				const endTime = Date.now()
				// Prefer the engine's wire time — the wall clock here also covers
				// connection setup and the disk write.
				const completion = parseCompletionPayload(event?.payload)
				completedOutputDirRef.current = completion?.outputDir ?? ''
				const duration =
					completion?.durationMs ??
					(transferStartTimeRef.current
						? endTime - transferStartTimeRef.current
						: 0)

				const currentFileNames = fileNamesRef.current
				const itemCount =
					transferItemCountRef.current ?? countTopLevelItems(currentFileNames)
				let displayName = t('common:receiver.downloadedFile')

				if (previewMetadataRef.current?.fileName) {
					displayName = previewMetadataRef.current.fileName
				} else if (currentFileNames.length > 0) {
					if (itemCount <= 1) {
						const fullPath = currentFileNames[0]
						displayName = fullPath.split('/').pop() || fullPath
					} else {
						const multipleFilesLabel = t('common:transfer.multipleFiles', {
							count: itemCount,
						})
						const firstPath = currentFileNames[0]
						const pathParts = firstPath.split('/')
						if (pathParts.length > 1) {
							displayName = pathParts[0] || multipleFilesLabel
						} else {
							displayName = multipleFilesLabel
						}
					}
				}

				let pathType: 'file' | 'directory' | null = null
				if (previewMetadataRef.current) {
					if (previewMetadataRef.current.mimeType === 'inode/directory') {
						pathType = 'directory'
					} else {
						pathType = 'file'
					}
				} else if (itemCount === 1 && currentFileNames.length > 1) {
					pathType = 'directory'
				} else if (itemCount === 1) {
					pathType = 'file'
				}

				const metadata = {
					fileName: displayName,
					fileSize: transferProgressRef.current?.totalBytes || 0,
					duration,
					startTime: transferStartTimeRef.current || endTime,
					endTime,
					downloadPath: savePathRef.current,
					itemCount: itemCount > 1 ? itemCount : undefined,
					pathType,
				}
				setTransferMetadata(metadata)

				if (pendingConflictNoticeRef.current) {
					showAlert(
						t('common:receiver.downloadCompletedWithConflicts'),
						pendingConflictNoticeRef.current,
						'info'
					)
					pendingConflictNoticeRef.current = null
				}

				void sendSystemNotification({
					title: t('common:receiver.downloadCompleted'),
					body: displayName,
				})
			})
		}

		setupListeners().catch((error) => {
			console.error('Failed to set up event listeners:', error)
		})

		return () => {
			disposed = true
			unlistenFns.forEach((unlisten) => {
				unlisten()
			})
		}
	}, [t, showAlert])

	const handleTicketChange = useCallback((newTicket: string) => {
		const fromLink = ticketFromReceiveLink(newTicket)
		setTicket(fromLink ?? newTicket)
	}, [])

	const handleBrowseFolder = useCallback(async () => {
		if (isReceiving) return
		try {
			let selected: string | null
			if (IS_ANDROID) {
				const response = await selectDownloadFolder()
				if (!response) return
				selected = response.path
				setDownloadsPath(selected)
				setDownloadsUri(response.uri)
			} else if (IS_IOS) {
				// The plugin holds the folder's security-scoped access, so Rust
				// writes to the plain path like on desktop.
				const response = await selectDownloadFolder()
				if (!response) return
				selected = response.path
			} else if (IS_WEB) {
				if (!supportsWebSaveLocationPicker()) {
					return
				}
				selected = await pickDownloadDirectory()
			} else {
				const dialogSelection = await openDialog({
					multiple: false,
					directory: true,
				})
				selected = Array.isArray(dialogSelection)
					? (dialogSelection[0] ?? null)
					: dialogSelection
			}

			if (selected) {
				setSavePath(selected)
			}
		} catch (error) {
			console.error('Failed to open folder dialog:', error)
			showAlert(
				t('common:errors.folderDialogFailed'),
				`${t('common:errors.folderDialogFailedDesc')}: ${error}`,
				'error'
			)
		}
	}, [isReceiving, setDownloadsPath, setDownloadsUri, showAlert, t])

	const receiveWithTicket = useCallback(
		async (ticketValue: string, subFolder?: string | null) => {
			if (!ticketValue.trim()) return

			try {
				if (transferItemCountRef.current == null) {
					transferItemCountRef.current =
						previewMetadataRef.current?.itemCount ?? previewMetadata?.itemCount
				}
				previewRequestSeqRef.current += 1
				transferSeqRef.current += 1
				setIsReceiving(true)
				setIsTransporting(false)
				setIsCompleted(false)
				setTransferMetadata(null)
				setTransferProgress(null)
				setTransferStartTime(null)
				setIsPreviewLoading(false)
				pendingConflictNoticeRef.current = null
				folderOpenTriggeredRef.current = false
				completedOutputDirRef.current = ''
				androidMediaStoreUrisRef.current = []
				androidMediaStorePathRef.current = ''
				// Every Android receive ends in an export, so the target is
				// pending from the moment the transfer starts.
				setIsExportPending(IS_ANDROID)
				androidOpenUriRef.current = IS_ANDROID
					? downloadsUriRef.current.trim()
					: ''

				let outputPath = savePathRef.current.trim()
				if (!outputPath && !IS_WEB && !IS_ANDROID) {
					outputPath = await downloadDir()
					setSavePath(outputPath)
					savePathRef.current = outputPath
				}

				await invoke<string>('receive_file', {
					ticket: ticketValue.trim(),
					outputPath,
					treeUri: IS_ANDROID ? downloadsUriRef.current.trim() || null : null,
					// Only the picker knows what the tree URI reads as, and
					// history needs it to name where the files ended up.
					treeDisplayPath: IS_ANDROID
						? downloadsPathRef.current.trim() || null
						: null,
					subFolder: subFolder?.trim() || null,
					relay: getRelayConfigArg(),
					discovery: getDiscoveryConfigArg(),
				})
			} catch (error) {
				// A receive that never finished never exports, so nothing is
				// coming to clear the pending flag.
				setIsExportPending(false)

				if (
					String(error) === 'cancelled' ||
					String(error).endsWith(': cancelled')
				)
					return

				console.error('Failed to receive file:', error)
				showAlert(
					t('common:errors.receiveFailed'),
					isWebPreviewError(error)
						? getWebPreviewErrorMessage(
								error,
								t('common:webPreview.transferUnavailable')
							)
						: String(error),
					'error'
				)
				setIsReceiving(false)
				setIsTransporting(false)
				setIsCompleted(false)
			}
		},
		[previewMetadata, showAlert, t]
	)

	const handleReceive = async () => {
		await receiveWithTicket(ticket)
	}

	// `isReceiving` stays true after a transfer finishes so the success screen
	// survives until the user clicks Done — see `isReceiveSessionBusy`. Reading
	// it directly here refused the next transfer until a screen was dismissed.
	const isSessionBusy = isReceiveSessionBusy({
		isReceiving,
		isTransporting,
		isCompleted,
		isExportPending,
	})

	const acceptPairedInvite = useCallback(
		async (
			invite: PairedInvitePayload,
			options?: AcceptPairedInviteOptions
		) => {
			if (isSessionBusy) {
				showAlert(
					t('common:receiver.receiveBusyTitle'),
					t('common:receiver.receiveBusyDescription'),
					'info'
				)
				return
			}

			useTransferTabStore.getState().requestTab('receive')

			const preview: TicketPreviewMetadata = {
				fileName: invite.sender_name,
				itemCount: invite.file_count,
				size: invite.total_size,
				mimeType:
					invite.file_count > 1 ? 'application/x-iroh-collection' : undefined,
			}
			setTicket(invite.blob_ticket)
			setPreviewMetadata(preview)
			previewMetadataRef.current = preview
			transferItemCountRef.current = invite.file_count
			previewRequestSeqRef.current += 1
			setIsPreviewLoading(false)

			await receiveWithTicket(invite.blob_ticket, options?.subFolder ?? null)
		},
		[isSessionBusy, receiveWithTicket, showAlert, t]
	)

	const registerAcceptPairedInvite = useReceiverActionsStore(
		(state) => state.registerAcceptPairedInvite
	)
	const registerBrowseSaveFolder = useReceiverActionsStore(
		(state) => state.registerBrowseSaveFolder
	)
	const setReceiverSavePath = useReceiverActionsStore(
		(state) => state.setReceiverSavePath
	)
	const setReceiverBusy = useReceiverActionsStore(
		(state) => state.setReceiverBusy
	)

	useEffect(() => {
		registerAcceptPairedInvite(acceptPairedInvite)
		return () => registerAcceptPairedInvite(null)
	}, [acceptPairedInvite, registerAcceptPairedInvite])

	useEffect(() => {
		registerBrowseSaveFolder(handleBrowseFolder)
		return () => registerBrowseSaveFolder(null)
	}, [handleBrowseFolder, registerBrowseSaveFolder])

	useEffect(() => {
		setReceiverSavePath(savePath)
	}, [savePath, setReceiverSavePath])

	useEffect(() => {
		setReceiverBusy(isSessionBusy)
	}, [isSessionBusy, setReceiverBusy])

	const resetForNewTransfer = async () => {
		// Zero the seq first so in-flight events from the cancelled transfer are ignored.
		transferSeqRef.current = 0
		previewRequestSeqRef.current += 1

		// Tell the backend to cancel the active download (idempotent if none active).
		invoke('cancel_receive').catch(() => {})

		setIsReceiving(false)
		setIsTransporting(false)
		setIsCompleted(false)
		setTicket('')
		setTransferMetadata(null)
		setTransferProgress(null)
		setTransferStartTime(null)
		setFileNames([])
		setPreviewMetadata(null)
		setIsPreviewLoading(false)
		pendingConflictNoticeRef.current = null
		folderOpenTriggeredRef.current = false
		androidOpenUriRef.current = ''
		androidMediaStoreUrisRef.current = []
		androidMediaStorePathRef.current = ''
		setIsExportPending(false)
		transferItemCountRef.current = undefined
	}

	const handleOpenFolder = async () => {
		// Files still in flight: no destination to send the user to. The button is
		// disabled while this holds, so reaching here means a stray call.
		if (IS_WEB || isExportPending || folderOpenTriggeredRef.current) {
			return
		}

		try {
			folderOpenTriggeredRef.current = true

			if (IS_ANDROID) {
				const treeUri = androidOpenUriRef.current.trim()
				if (treeUri) {
					await openDownloadFolder(treeUri)
					return
				}

				const mediaStoreUris = androidMediaStoreUrisRef.current
				const mediaStorePath = androidMediaStorePathRef.current
				if (mediaStoreUris.length > 0 || mediaStorePath) {
					// One file opens in whatever app handles it; several open
					// the folder they landed in.
					await openDownloadTarget(
						mediaStoreUris.length === 1 ? mediaStoreUris[0] : '',
						mediaStorePath
					)
					return
				}

				folderOpenTriggeredRef.current = false
				showAlert(
					t('common:errors.openFolderFailed'),
					t('common:errors.openFolderUnavailableDesc'),
					'error'
				)
				return
			}

			const revealBase = completedOutputDirRef.current || savePath
			if (!revealBase) {
				folderOpenTriggeredRef.current = false
				return
			}

			const targetPath = await resolveRevealPath(
				revealBase,
				fileNamesRef.current
			)
			if (targetPath) {
				// The opener plugin can't reveal files on iOS, so open the item
				// itself, or the folder when several top-level items arrived.
				if (IS_IOS) {
					const topLevel = new Set(
						fileNamesRef.current.map((name) =>
							isAbsolutePath(name)
								? name
								: normalizeSeparators(name).split('/')[0]
						)
					)
					await openDownloadTarget(topLevel.size > 1 ? revealBase : targetPath)
				} else await revealItemInDir(targetPath)
			}
		} catch (error) {
			folderOpenTriggeredRef.current = false
			console.error('Failed to open download folder:', error)
			showAlert(
				t('common:errors.openFolderFailed'),
				`${t('common:errors.openFolderFailedDesc')}: ${error}`,
				'error'
			)
		}
	}

	return {
		ticket,
		isReceiving,
		isTransporting,
		isCompleted,
		savePath,
		alertDialog,
		transferMetadata,
		transferProgress,
		previewMetadata,
		isPreviewLoading,
		isExportPending,
		fileNames,

		handleTicketChange,
		handleBrowseFolder,
		handleReceive,
		handleOpenFolder,
		showAlert,
		closeAlert,
		resetForNewTransfer,
	}
}
