import { useQuery } from '@tanstack/react-query'
import { invoke } from '@/lib/platform-api'
import { IS_LINUX, IS_TAURI, IS_WINDOWS } from '@/lib/platform'

export const contextMenuQueryKeys = {
	supported: () => ['context-menu-supported'] as const,
}

/** Desktops where a file manager right-click entry exists at all. */
export const HAS_CONTEXT_MENU_PLATFORM = IS_TAURI && (IS_WINDOWS || IS_LINUX)

/**
 * Whether the file manager right-click entry can be offered: always Explorer on
 * Windows; on Linux only when GNOME Files (Nautilus) is detected. Cached for the
 * app lifetime — the desktop does not change mid-session.
 */
export function useIsContextMenuSupported() {
	return useQuery({
		queryKey: contextMenuQueryKeys.supported(),
		queryFn: async () => {
			if (!HAS_CONTEXT_MENU_PLATFORM) {
				return false
			}
			return invoke<boolean>('is_context_menu_supported')
		},
		staleTime: Number.POSITIVE_INFINITY,
		gcTime: Number.POSITIVE_INFINITY,
		retry: false,
	})
}
