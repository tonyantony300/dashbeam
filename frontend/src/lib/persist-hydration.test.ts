import assert from 'node:assert/strict'
import { describe, it } from 'node:test'
import { whenHydrated } from './persist-hydration.js'

function fakePersist(hydrated: boolean) {
	const listeners = new Set<() => void>()
	return {
		listeners,
		api: {
			hasHydrated: () => hydrated,
			onFinishHydration: (fn: () => void) => {
				listeners.add(fn)
				return () => listeners.delete(fn)
			},
		},
		finish() {
			hydrated = true
			for (const fn of [...listeners]) fn()
		},
	}
}

describe('whenHydrated', () => {
	it('resolves right away when the store already loaded from disk', async () => {
		const { api, listeners } = fakePersist(true)
		await whenHydrated(api)
		assert.equal(listeners.size, 0)
	})

	// Reading settings before this point returns in-memory defaults, which is
	// how custom relay/discovery got replaced by n0's on startup (#349).
	it('waits for hydration to finish, then unsubscribes', async () => {
		const persist = fakePersist(false)
		let resolved = false
		const pending = whenHydrated(persist.api).then(() => {
			resolved = true
		})

		await Promise.resolve()
		assert.equal(resolved, false)

		persist.finish()
		await pending
		assert.equal(resolved, true)
		assert.equal(persist.listeners.size, 0)
	})
})
