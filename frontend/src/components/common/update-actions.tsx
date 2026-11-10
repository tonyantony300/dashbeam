import { Button } from '@/components/ui/button'
import { useRestartToUpdate } from '@/hooks/use-updater'
import { useTranslation } from '@/i18n'
import { openExternalUrl } from '@/lib/openExternalUrl'
import { DOWNLOAD_PAGE_LINK } from '@/lib/version'
import { useUpdaterStore } from '@/store/updater-store'

/**
 * The "Restart to update" step, including the in-flight-transfer warning.
 * Every surface offering the restart renders this, so none can skip the
 * warning.
 */
export function RestartToUpdate() {
	const { t } = useTranslation()
	const { confirming, requestRestart, cancel } = useRestartToUpdate()

	return (
		<div className="flex flex-col gap-2">
			{confirming && (
				<p className="text-destructive text-xs">
					{t('updater.restartBusyDescription')}
				</p>
			)}
			<div className="flex justify-end gap-2">
				{confirming && (
					<Button variant="outline" size="sm" onClick={cancel}>
						{t('updater.cancel')}
					</Button>
				)}
				<Button
					size="sm"
					variant={confirming ? 'destructive' : 'default'}
					onClick={requestRestart}
				>
					{confirming
						? t('updater.restartAnyway')
						: t('updater.restartToUpdate')}
				</Button>
			</div>
		</div>
	)
}

/** Why the last download or install failed, with a way out of the app. */
export function UpdateError() {
	const { t } = useTranslation()
	const error = useUpdaterStore((s) => s.error)
	if (!error) return null

	return (
		<div role="alert" className="flex flex-col items-start gap-1">
			<p className="break-words text-destructive text-xs">
				{t('updater.updateFailedReason', { reason: error })}
			</p>
			<Button
				variant="link"
				size="xs"
				className="h-auto px-0"
				onClick={() => void openExternalUrl(DOWNLOAD_PAGE_LINK)}
			>
				{t('updater.downloadManually')}
			</Button>
		</div>
	)
}
