import { useEffect } from 'react'
import { Gift, Loader2, X } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useTranslation } from '@/i18n'
import { useAppSettingStore } from '@/store/app-setting'
import { useUpdaterStore } from '@/store/updater-store'
import {
	useAnnounceInstalledUpdate,
	useInstallUpdate,
	useUpdaterSync,
} from '@/hooks/use-updater'
import { RestartToUpdate, UpdateError } from './update-actions'
import { UpdateProgressBar } from './update-progress'
import { WhatsNewDialog } from './WhatsNewDialog'

export function AppUpdater() {
	const { t } = useTranslation()
	const autoUpdate = useAppSettingStore((state) => state.autoUpdate)
	useUpdaterSync(autoUpdate)
	useAnnounceInstalledUpdate()

	const phase = useUpdaterStore((s) => s.phase)
	const version = useUpdaterStore((s) => s.version)
	const bannerVisible = useUpdaterStore((s) => s.bannerVisible)
	const downloadUrl = useUpdaterStore((s) => s.downloadUrl)
	const downloadedBytes = useUpdaterStore((s) => s.downloadedBytes)
	const contentLength = useUpdaterStore((s) => s.contentLength)
	const progressRatio = useUpdaterStore((s) => s.progressRatio)
	const dismiss = useUpdaterStore((s) => s.dismiss)
	const whatsNewOpen = useUpdaterStore((s) => s.whatsNew !== null)
	const openWhatsNew = useUpdaterStore((s) => s.openWhatsNew)

	const { install } = useInstallUpdate()

	const canDismiss = phase === 'available'

	// Esc closes the banner the way any dismissible surface should. The old
	// AlertDialog swallowed Esc entirely.
	useEffect(() => {
		// With the dialog open, Esc belongs to the dialog.
		if (!bannerVisible || !canDismiss || whatsNewOpen) return
		const onKeyDown = (event: KeyboardEvent) => {
			if (event.key === 'Escape') dismiss()
		}
		window.addEventListener('keydown', onKeyDown)
		return () => window.removeEventListener('keydown', onKeyDown)
	}, [bannerVisible, canDismiss, whatsNewOpen, dismiss])

	// Deliberately not a dialog: no backdrop, no focus trap, no viewport. The
	// rest of the window stays clickable while this sits in the corner. It
	// clears the footer rather than covering it — on a phone the footer
	// carries the safe-area inset, so the nav would be underneath.
	return (
		<>
			<WhatsNewDialog />
			{bannerVisible && (
				<section
					aria-label={t('updater.newUpdateTitle')}
					className="fixed bottom-[calc(3.25rem+var(--safe-area-bottom))] left-3 z-40 w-[22rem] max-w-[calc(100vw-1.5rem)] rounded-xl border bg-popover p-4 text-popover-foreground shadow-lg"
				>
					{canDismiss && (
						<Button
							variant="ghost"
							size="icon"
							className="absolute top-2 right-2 size-6 text-muted-foreground"
							aria-label={t('updater.dismiss')}
							onClick={dismiss}
						>
							<X className="size-3.5" />
						</Button>
					)}

					<div className="flex items-center gap-2 pr-6">
						<Gift className="size-4 shrink-0 text-muted-foreground" />
						<p aria-live="polite" className="text-sm text-muted-foreground">
							{phase === 'available' &&
								t('updater.newVersionAvailableInline', { version })}
							{phase === 'downloading' &&
								t('updater.downloadingTitle', { version })}
							{phase === 'installing' && t('updater.installingTitle')}
							{phase === 'ready' && t('updater.readyTitle', { version })}
							{phase === 'restarting' && t('updater.restarting')}
						</p>
					</div>

					{phase === 'downloading' && (
						<div className="mt-3">
							<UpdateProgressBar
								downloadedBytes={downloadedBytes}
								contentLength={contentLength}
								progressRatio={progressRatio}
							/>
						</div>
					)}

					{phase === 'installing' && (
						<div className="mt-3">
							<UpdateProgressBar
								downloadedBytes={downloadedBytes}
								contentLength={null}
								progressRatio={null}
							/>
						</div>
					)}

					{(phase === 'available' || phase === 'ready') && (
						<div className="mt-2 empty:hidden">
							<UpdateError />
						</div>
					)}

					{phase === 'available' && (
						<div className="mt-3 flex items-center justify-end gap-2">
							{version && (
								<Button
									variant="link"
									size="sm"
									className="me-auto px-0"
									onClick={() => openWhatsNew({ version, installed: false })}
								>
									{t('updater.whatsNew')}
								</Button>
							)}
							<Button variant="outline" size="sm" onClick={dismiss}>
								{t('updater.later')}
							</Button>
							<Button size="sm" onClick={() => void install()}>
								{downloadUrl ? t('updater.download') : t('updater.updateNow')}
							</Button>
						</div>
					)}

					{phase === 'ready' && (
						<div className="mt-3">
							<RestartToUpdate />
						</div>
					)}

					{phase === 'restarting' && (
						<div className="mt-3 flex justify-end">
							<Loader2 className="size-4 animate-spin text-muted-foreground" />
						</div>
					)}
				</section>
			)}
		</>
	)
}
