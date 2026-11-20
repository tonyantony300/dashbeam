import { useQuery } from '@tanstack/react-query'
import { Loader2 } from 'lucide-react'
import { useState } from 'react'
import { Button } from '@/components/ui/button'
import {
	Dialog,
	DialogFooter,
	DialogHeader,
	DialogPanel,
	DialogPopup,
	DialogTitle,
} from '@/components/ui/dialog'
import { releaseNotesQueryOptions, useInstallUpdate } from '@/hooks/use-updater'
import { useTranslation } from '@/i18n'
import { openExternalUrl } from '@/lib/openExternalUrl'
import { releasePageUrl } from '@/lib/release-notes'
import { useUpdaterStore } from '@/store/updater-store'
import { ReleaseNotesView } from './release-notes-view'

/**
 * Release notes for one version, opened from the store: before updating (with
 * an "Update now" button) and once after, on the first launch of the new
 * version. Mounted once, by `AppUpdater`.
 */
export function WhatsNewDialog() {
	const { t } = useTranslation()
	const target = useUpdaterStore((s) => s.whatsNew)
	const close = useUpdaterStore((s) => s.closeWhatsNew)
	const phase = useUpdaterStore((s) => s.phase)
	const pendingVersion = useUpdaterStore((s) => s.version)
	const manifestNotes = useUpdaterStore((s) => s.notes)
	const downloadUrl = useUpdaterStore((s) => s.downloadUrl)
	const { install } = useInstallUpdate()

	// Keep the last target so the content doesn't blank during the exit
	// animation.
	const [shown, setShown] = useState(target)
	if (target && target !== shown) setShown(target)

	const version = shown?.version ?? ''
	const notes = useQuery({
		...releaseNotesQueryOptions(version),
		enabled: target !== null,
	})

	// GitHub's hand-edited notes first; whatever the update manifest carried
	// (Android's check, or `latest.json`) if GitHub can't be reached.
	const fallback =
		shown && !shown.installed && shown.version === pendingVersion
			? manifestNotes
			: null
	const markdown = notes.data?.body ?? (notes.isError ? fallback : null)
	const releaseUrl = notes.data?.url ?? releasePageUrl(version)
	const canUpdate =
		shown !== null &&
		!shown.installed &&
		phase === 'available' &&
		shown.version === pendingVersion

	return (
		<Dialog
			open={target !== null}
			onOpenChange={(open) => {
				if (!open) close()
			}}
		>
			<DialogPopup>
				<DialogHeader>
					<DialogTitle>{t('updater.whatsNewTitle', { version })}</DialogTitle>
				</DialogHeader>
				<DialogPanel>
					{markdown ? (
						<ReleaseNotesView markdown={markdown} />
					) : notes.isError ? (
						<p className="text-muted-foreground text-sm">
							{t('updater.whatsNewUnavailable')}
						</p>
					) : (
						<p className="flex items-center gap-2 text-muted-foreground text-sm">
							<Loader2 className="size-4 animate-spin" />
							{t('updater.whatsNewLoading')}
						</p>
					)}
				</DialogPanel>
				<DialogFooter>
					<Button
						variant="outline"
						onClick={() => void openExternalUrl(releaseUrl)}
					>
						{t('updater.viewOnGithub')}
					</Button>
					{canUpdate ? (
						<Button
							onClick={() => {
								close()
								void install()
							}}
						>
							{downloadUrl ? t('updater.download') : t('updater.updateNow')}
						</Button>
					) : (
						<Button onClick={close}>{t('updater.close')}</Button>
					)}
				</DialogFooter>
			</DialogPopup>
		</Dialog>
	)
}
