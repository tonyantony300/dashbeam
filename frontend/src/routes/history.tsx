import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { History as HistoryIcon, Loader2 } from 'lucide-react'
import { useTranslation } from '../i18n'
import { TransferHistoryRow } from '../components/history/TransferHistoryRow'
import { BackArrowIcon } from '../components/back-arrow-icon'
import {
	AlertDialog,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from '../components/ui/alert-dialog'
import { Button } from '../components/ui/button'
import {
	Empty,
	EmptyDescription,
	EmptyHeader,
	EmptyMedia,
	EmptyTitle,
} from '../components/ui/empty'
import { Frame } from '../components/ui/frame'
import { ScrollArea } from '../components/ui/scroll-area'
import {
	Select,
	SelectItem,
	SelectPopup,
	SelectTrigger,
	SelectValue,
} from '../components/ui/select'
import { toastManager } from '../components/ui/toast'
import { listPairedDevices } from '../lib/pairing-api'
import { IS_ANDROID, IS_IOS, IS_PAIRING_CAPABLE } from '../lib/platform'
import { listen, revealItemInDir, type UnlistenFn } from '../lib/platform-api'
import { resolveAndroidOpenTarget } from '../lib/history-open-target'
import { openDownloadFolder, openDownloadTarget } from '../plugins/nativeUtils'
import { AppAlertDialog } from '../components/AppAlertDialog'
import { formatFileSize } from '../lib/utils'
import { formatReceiveSavePath } from '../lib/receive-save-path'
import {
	filterTransferHistory,
	TRANSFER_STATUS_FILTERS,
	type TransferRecord,
	type TransferStatusFilter,
} from '../lib/transfer-history'
import {
	clearTransferHistory,
	clearTransferTempData,
	deleteTransferRecord,
	getTransferTempData,
	listTransferHistory,
	transferOpenTarget,
	type TransferTempData,
} from '../lib/transfer-history-api'

interface KnownDevice {
	endpoint_id: string
	display_name: string
}

export function HistoryPage() {
	const { t } = useTranslation()
	const [records, setRecords] = useState<TransferRecord[]>([])
	const [pairedDevices, setPairedDevices] = useState<KnownDevice[]>([])
	const [tempData, setTempData] = useState<Record<string, TransferTempData>>({})
	const [status, setStatus] = useState<TransferStatusFilter>('all')
	const [isLoading, setIsLoading] = useState(true)
	const [isBusy, setIsBusy] = useState(false)
	const [confirmClearAll, setConfirmClearAll] = useState(false)
	const [pendingRemove, setPendingRemove] = useState<TransferRecord | null>(
		null
	)
	const [missingTarget, setMissingTarget] = useState<TransferRecord | null>(
		null
	)

	const refresh = useCallback(async () => {
		const rows = await listTransferHistory()
		setRecords(rows)

		// Stat'd live, not trusted from the row — a store can vanish any time.
		const reclaimable = rows.filter((row) => row.resumableStorePath)
		const stats = await Promise.all(
			reclaimable.map(async (row) => {
				try {
					return [row.id, await getTransferTempData(row.id)] as const
				} catch {
					return null
				}
			})
		)
		setTempData(Object.fromEntries(stats.filter((entry) => entry !== null)))
	}, [])

	useEffect(() => {
		let cancelled = false
		const load = async () => {
			try {
				const devices = IS_PAIRING_CAPABLE
					? await listPairedDevices().catch(() => [])
					: []
				if (!cancelled) {
					setPairedDevices(devices as KnownDevice[])
				}
				await refresh()
			} catch (error) {
				console.error('Failed to load transfer history:', error)
			} finally {
				if (!cancelled) {
					setIsLoading(false)
				}
			}
		}
		void load()
		return () => {
			cancelled = true
		}
	}, [refresh])

	// A trusted device can land a transfer while this list is open. The shell
	// announces a row only once it is terminal, so this re-reads the store
	// rather than tracking transfer state of its own.
	useEffect(() => {
		let cancelled = false
		let unlisten: UnlistenFn | undefined
		void listen('transfer-history-updated', () => {
			refresh().catch((error) => {
				console.error('Failed to refresh transfer history:', error)
			})
		}).then((fn) => {
			// Resolved after unmount: nothing left to notify, so drop it.
			if (cancelled) {
				fn()
				return
			}
			unlisten = fn
		})
		return () => {
			cancelled = true
			unlisten?.()
		}
	}, [refresh])

	const visible = useMemo(
		() => filterTransferHistory(records, status),
		[records, status]
	)

	const handleRemove = async (id: string) => {
		setIsBusy(true)
		try {
			await deleteTransferRecord(id)
			await refresh()
			setPendingRemove(null)
		} catch (error) {
			console.error(error)
			toastManager.add({
				title: t('common:history.removeFailed'),
				type: 'error',
			})
		} finally {
			setIsBusy(false)
		}
	}

	/**
	 * Android paths are labels rather than locations, so there the row's own
	 * recorded target is followed and the system reports what it cannot open.
	 * Everywhere else the destination is checked first: a folder that outlived
	 * the files it held would otherwise open to nothing.
	 */
	const handleOpen = async (record: TransferRecord) => {
		try {
			if (IS_ANDROID) {
				const target = resolveAndroidOpenTarget(record)
				if (!target) {
					setMissingTarget(record)
					return
				}
				if (target.kind === 'folder') {
					await openDownloadFolder(target.treeUri)
				} else {
					await openDownloadTarget('', target.relativePath)
				}
				return
			}

			const path = await transferOpenTarget(record.id)
			if (!path) {
				setMissingTarget(record)
				return
			}
			// The opener plugin can't reveal files on iOS.
			if (IS_IOS) await openDownloadTarget(path)
			else await revealItemInDir(path)
		} catch (error) {
			console.error(error)
			toastManager.add({
				title: t('common:history.row.openFailed'),
				type: 'error',
			})
		}
	}

	const handleClearTempData = async (id: string) => {
		setIsBusy(true)
		try {
			await clearTransferTempData(id)
			await refresh()
			toastManager.add({
				title: t('common:history.temp.cleared'),
				type: 'success',
			})
		} catch (error) {
			console.error(error)
			toastManager.add({
				title:
					error instanceof Error
						? error.message
						: t('common:history.temp.clearFailed'),
				type: 'error',
			})
		} finally {
			setIsBusy(false)
		}
	}

	const handleClearAll = async () => {
		setIsBusy(true)
		try {
			await clearTransferHistory()
			await refresh()
			setConfirmClearAll(false)
			toastManager.add({
				title: t('common:history.clearAllDone'),
				type: 'success',
			})
		} catch (error) {
			console.error(error)
			toastManager.add({
				title: t('common:history.clearAllFailed'),
				type: 'error',
			})
		} finally {
			setIsBusy(false)
		}
	}

	return (
		<div className="flex min-h-0 flex-1 flex-col">
			{/* RootLayout paints a `z-10` drag region over the top 40px on macOS:
			    `z-20` lets presses reach these controls, `pointer-events-none`
			    keeps the gap between them draggable. */}
			<div className="pointer-events-none relative z-20 flex items-center justify-between gap-2 pt-[calc(1.5rem+var(--safe-area-top))] pl-[calc(1rem+var(--safe-area-left))] pr-[calc(1rem+var(--safe-area-right))]">
				{/* Arrow and title are one target, matching the settings header. */}
				<Link
					to="/"
					className="pointer-events-auto flex min-w-0 items-center gap-2 rounded-md px-1.5 outline-none focus-visible:ring-2 focus-visible:ring-ring"
				>
					<BackArrowIcon size={18} className="text-muted-foreground" />
					<h1 className="truncate font-medium text-lg">
						{t('common:history.title')}
					</h1>
				</Link>
				{records.length > 0 && (
					<Button
						className="pointer-events-auto"
						variant="outline"
						size="xs"
						disabled={isBusy}
						onClick={() => setConfirmClearAll(true)}
					>
						{t('common:history.clearAll')}
					</Button>
				)}
			</div>

			<div className="flex min-h-0 flex-1 flex-col gap-3 pt-3 pl-[calc(1rem+var(--safe-area-left))] pr-[calc(1rem+var(--safe-area-right))]">
				{/* A dropdown, not five chips: status is usually left on "All".
				    Direction isn't a filter — one timeline, each row has its arrow. */}
				<div className="flex items-center gap-2">
					<Select value={status}>
						<SelectTrigger size="sm" className="w-44">
							<SelectValue placeholder={t('common:history.filters.all')}>
								{(value: string | null) =>
									t(`common:history.filters.${value ?? 'all'}`)
								}
							</SelectValue>
						</SelectTrigger>
						<SelectPopup>
							{TRANSFER_STATUS_FILTERS.map((option) => (
								<SelectItem
									key={option}
									value={option}
									onClick={() => setStatus(option)}
								>
									{t(`common:history.filters.${option}`)}
								</SelectItem>
							))}
						</SelectPopup>
					</Select>
					<span className="text-muted-foreground text-sm">
						{t('common:history.filters.count', { count: visible.length })}
					</span>
				</div>

				<div className="min-h-0 flex-1">
					<ScrollArea className="h-full">
						{isLoading ? (
							<div
								role="status"
								aria-busy="true"
								className="flex items-center gap-2 py-8 text-muted-foreground text-sm"
							>
								<Loader2 className="h-4 w-4 animate-spin" />
								{t('common:loading')}
							</div>
						) : visible.length === 0 ? (
							<Empty className="py-10">
								<EmptyHeader>
									<EmptyMedia variant="icon">
										<HistoryIcon />
									</EmptyMedia>
									<EmptyTitle>{t('common:history.empty.title')}</EmptyTitle>
									<EmptyDescription>
										{status === 'all'
											? t('common:history.empty.description')
											: t('common:history.empty.filtered')}
									</EmptyDescription>
								</EmptyHeader>
							</Empty>
						) : (
							<Frame className="mb-12">
								{visible.map((record) => (
									<TransferHistoryRow
										key={record.id}
										record={record}
										pairedDevices={pairedDevices}
										tempData={tempData[record.id]}
										onOpen={handleOpen}
										onRemove={setPendingRemove}
										onClearTempData={handleClearTempData}
										isBusy={isBusy}
									/>
								))}
							</Frame>
						)}
					</ScrollArea>
				</div>
			</div>

			<AlertDialog open={confirmClearAll} onOpenChange={setConfirmClearAll}>
				<AlertDialogContent>
					<AlertDialogHeader>
						<AlertDialogTitle>
							{t('common:history.clearAllConfirmTitle')}
						</AlertDialogTitle>
						<AlertDialogDescription>
							{t('common:history.clearAllConfirmBody')}
						</AlertDialogDescription>
					</AlertDialogHeader>
					<AlertDialogFooter>
						<Button
							variant="outline"
							onClick={() => setConfirmClearAll(false)}
							disabled={isBusy}
						>
							{t('common:cancel')}
						</Button>
						<Button
							variant="destructive"
							onClick={handleClearAll}
							disabled={isBusy}
						>
							{t('common:history.clearAll')}
						</Button>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>

			<AlertDialog
				open={pendingRemove !== null}
				onOpenChange={(open) => {
					if (!open) setPendingRemove(null)
				}}
			>
				<AlertDialogContent>
					<AlertDialogHeader>
						<AlertDialogTitle>
							{t('common:history.row.removeConfirmTitle')}
						</AlertDialogTitle>
						{/* Deleting a row also reclaims its partial store, so a row
						    holding one gets the stronger warning. */}
						<AlertDialogDescription>
							{pendingRemove && tempData[pendingRemove.id]?.exists
								? t('common:history.row.removeConfirmBodyResumable', {
										size: formatFileSize(tempData[pendingRemove.id].sizeBytes),
									})
								: t('common:history.row.removeConfirmBody')}
						</AlertDialogDescription>
					</AlertDialogHeader>
					<AlertDialogFooter>
						<Button
							variant="outline"
							onClick={() => setPendingRemove(null)}
							disabled={isBusy}
						>
							{t('common:cancel')}
						</Button>
						<Button
							variant="destructive"
							onClick={() => {
								if (pendingRemove) void handleRemove(pendingRemove.id)
							}}
							disabled={isBusy}
						>
							{t('common:history.row.remove')}
						</Button>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>

			<AppAlertDialog
				isOpen={missingTarget !== null}
				title={t('common:history.row.missingTitle')}
				description={t('common:history.row.missingBody', {
					path:
						formatReceiveSavePath(missingTarget?.savePath) ||
						t('common:history.row.unknownDestination'),
				})}
				type="error"
				onClose={() => setMissingTarget(null)}
			/>
		</div>
	)
}
