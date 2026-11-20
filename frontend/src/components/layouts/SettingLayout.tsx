import { motion } from 'motion/react'
import { Outlet, useLocation } from 'react-router-dom'
import { IS_MOBILE, IS_WEB } from '@/lib/platform'
import { cn } from '@/lib/utils'
import { RelayChangeGuard } from '../settings/relay'
import SettingSidebar from '../setting-sidebar'
import { ScrollArea } from '../ui/scroll-area'
import { SidebarProvider, SidebarInset } from '../ui/sidebar'

export function SettingLayout() {
	const location = useLocation()

	return (
		// On mobile the sidebar wrapper's default full viewport height pushes
		// RootLayout's footer off the bottom of the screen: share the space instead.
		<SidebarProvider
			className={
				IS_WEB ? 'h-full min-h-0' : IS_MOBILE ? 'min-h-0 flex-1' : undefined
			}
		>
			<SettingSidebar />
			<SidebarInset className="min-h-0">
				{/* Pin the scrollbar visible (opacity-0 until hover by default) so the
				    overflow is always obvious. */}
				<ScrollArea className="[&_[data-slot=scroll-area-scrollbar][data-orientation=vertical]]:opacity-100">
					<motion.div
						key={location.pathname}
						// The settings pane spans the viewport's width on native, so its
						// sides are the screen edges: pad the content past the system bars.
						// The footer below it already clears the bottom one.
						className={cn(
							'flex flex-col gap-4 pt-4 outline-none',
							'pl-[calc(1rem+var(--safe-area-left))] pr-[calc(1rem+var(--safe-area-right))]',
							'pb-12'
						)}
						initial={{ opacity: 0 }}
						animate={{ opacity: 1 }}
						transition={{ duration: 0.3, ease: 'easeOut' }}
					>
						<Outlet />
					</motion.div>
				</ScrollArea>
			</SidebarInset>
			<RelayChangeGuard />
		</SidebarProvider>
	)
}
