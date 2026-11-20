/** The slice of zustand's `persist` API needed to wait for rehydration. */
export type PersistHydrationApi = {
	hasHydrated: () => boolean
	onFinishHydration: (fn: () => void) => () => void
}

/** Resolves once a persisted store has loaded from disk (async on Tauri). */
export function whenHydrated(persist: PersistHydrationApi): Promise<void> {
	if (persist.hasHydrated()) return Promise.resolve()
	return new Promise((resolve) => {
		const unsubscribe = persist.onFinishHydration(() => {
			unsubscribe()
			resolve()
		})
	})
}
