import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { IS_ANDROID, IS_DESKTOP, IS_IOS, IS_TAURI } from '@/lib/platform'
import { invoke } from '@/lib/platform-api'
import type { RelayConfigArg } from '@/lib/relay-config'

export type DebugLoggingState = {
	/** Persisted toggle — takes effect on the next launch. */
	enabled: boolean
	/** Whether the file sink was actually installed at startup this run. */
	activeThisSession: boolean
	logDir: string | null
}

const DISABLED: DebugLoggingState = {
	enabled: false,
	activeThisSession: false,
	logDir: null,
}

export const debugLoggingQueryKeys = {
	state: () => ['debug-logging'] as const,
}

export const DEBUG_LOGGING_SUPPORTED =
	IS_TAURI && (IS_DESKTOP || IS_ANDROID || IS_IOS)

/**
 * Verbosity is fixed at startup, so `enabled` and `activeThisSession` disagree
 * between toggling and restarting — that gap drives the "restart required" hint.
 */
export function useDebugLogging() {
	return useQuery({
		queryKey: debugLoggingQueryKeys.state(),
		queryFn: async () => {
			if (!DEBUG_LOGGING_SUPPORTED) return DISABLED
			return invoke<DebugLoggingState>('get_debug_logging')
		},
		staleTime: Number.POSITIVE_INFINITY,
		gcTime: Number.POSITIVE_INFINITY,
		retry: false,
	})
}

export function useSetDebugLogging() {
	const queryClient = useQueryClient()
	return useMutation({
		mutationFn: (enabled: boolean) =>
			invoke<void>('set_debug_logging', { enabled }),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: debugLoggingQueryKeys.state() })
		},
	})
}

export function useExportDebugBundle() {
	return useMutation({
		// Only the frontend's settings store knows which relay the user selected.
		mutationFn: ({
			destPath,
			relay,
		}: {
			destPath: string
			relay: RelayConfigArg | null
		}) => invoke<void>('export_debug_bundle', { destPath, relay }),
	})
}

export function useClearDebugLogs() {
	return useMutation({
		mutationFn: () => invoke<void>('clear_debug_logs'),
	})
}
