import { useTranslation } from '../../../i18n'
import { IS_MACOS } from '@/lib/platform'
import { useIsContextMenuSupported } from '@/hooks/use-context-menu-support'
import { Frame, FrameHeader, FramePanel, FrameTitle } from '../../ui/frame'
import { ContextMenuToggle } from './context-menu-toggle'
import { MinimizeSystemTray } from './minimize-system-tray'
import { StartOnStartup } from './start-on-startup'

export function SystemTray() {
	const { t } = useTranslation()
	const { data: contextMenuSupported = false } = useIsContextMenuSupported()
	return (
		<Frame>
			<FrameHeader>
				<FrameTitle>{t('settings.general.systembar.title')}</FrameTitle>
			</FrameHeader>
			<FramePanel className="space-y-4">
				<StartOnStartup />
				{/* macOS never quits on window close, so there is nothing to decide. */}
				{!IS_MACOS && <MinimizeSystemTray />}
				{/* `ShowProgressOnIcon` is not rendered — nothing implements
				    progress-on-icon yet. Kept so restoring it is a one-liner. */}
				{contextMenuSupported && <ContextMenuToggle />}
			</FramePanel>
		</Frame>
	)
}
