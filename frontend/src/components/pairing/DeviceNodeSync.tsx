import { useEffect, useRef } from 'react'
import { listen } from '@/lib/platform-api'
import { IS_PAIRING_CAPABLE } from '@/lib/platform'
import { getRelayConfigArg } from '@/lib/relay'
import { getDiscoveryConfigArg } from '@/lib/discovery'
import { whenHydrated } from '@/lib/persist-hydration'
import {
	isKnownPairedEndpoint,
	reconfigureNodeRelay,
	setDiscoverability,
} from '@/lib/pairing-api'
import { shouldAutoAccept } from '@/lib/auto-accept'
import { useAppSettingStore } from '@/store/app-setting'
import type {
	PairedInvitePayload,
	PairedInviteResponsePayload,
} from '@/lib/pairing-api'
import { usePairedInviteStore } from '@/store/paired-invite-store'
import {
	usePairingDataStore,
	preloadPairingData,
	pairingDataHydrated,
} from '@/store/pairing-data-store'
import { ensureNodeCapabilityLifecycle } from '@/store/node-capability-store'
import { useNodeCapability } from '@/hooks/useNodeCapability'
import { useInviteNotifications } from '@/hooks/useInviteNotifications'
import { ensureNotificationPermission } from '@/lib/systemNotification'
import { useTranslation } from '@/i18n'
import { toastManager } from '../ui/toast'
import { useAutoAcceptQueueStore } from '@/store/auto-accept-queue-store'
import { useAutoAcceptQueue } from '@/hooks/useAutoAcceptQueue'

/** Syncs relay settings to the device node and listens for paired invites globally. */
export function DeviceNodeSync() {
	const { t } = useTranslation()
	useInviteNotifications()
	useAutoAcceptQueue()
	const { isNodeReady, refreshNodeStatus } = useNodeCapability()
	const setInvite = usePairedInviteStore((s) => s.setInvite)
	const didSyncRelay = useRef(false)

	// Warm node status + devices/this-device before settings opens, so the
	// first Devices visit paints complete content instead of loading → ready.
	useEffect(() => {
		if (!IS_PAIRING_CAPABLE) return
		ensureNodeCapabilityLifecycle()
		void preloadPairingData()
		// Asked while the app is on screen: a backgrounded Activity can't show
		// Android's POST_NOTIFICATIONS dialog. Self-guards to once per session.
		void ensureNotificationPermission()
	}, [])

	// Preload may finish while the node is still starting; hydrate once ready.
	useEffect(() => {
		if (!IS_PAIRING_CAPABLE || !isNodeReady) return
		void usePairingDataStore.getState().hydrate()
	}, [isNodeReady])

	useEffect(() => {
		if (!IS_PAIRING_CAPABLE || !isNodeReady || didSyncRelay.current) return
		didSyncRelay.current = true
		// `init_node_service` already applied these from disk; this is a safety
		// net and applies relay `public` fallback. Settings load asynchronously,
		// so reading them before hydration would push the in-memory defaults
		// (public relays, discoverable to everyone) onto the node.
		const hydrated = whenHydrated(useAppSettingStore.persist)
		void hydrated
			.then(() =>
				reconfigureNodeRelay(getRelayConfigArg(), getDiscoveryConfigArg())
			)
			.catch((error) => {
				// Allow a later retry if the first sync failed (e.g. node still settling).
				didSyncRelay.current = false
				console.warn('Failed to sync node relay on startup:', error)
			})
		void hydrated
			.then(() =>
				setDiscoverability(useAppSettingStore.getState().discoverability)
			)
			.catch((error) => {
				console.warn('Failed to sync discoverability on startup:', error)
			})
	}, [isNodeReady])

	useEffect(() => {
		if (!IS_PAIRING_CAPABLE) return

		let disposed = false
		let unlistenInvite: (() => void) | undefined
		let unlistenResponse: (() => void) | undefined
		let unlistenExpired: (() => void) | undefined

		const setup = async () => {
			const inviteUnlisten = await listen(
				'paired-invite-received',
				(event: { payload: unknown }) => {
					let payload: PairedInvitePayload
					try {
						payload = JSON.parse(String(event.payload)) as PairedInvitePayload
					} catch {
						// Ignore malformed invite payloads
						return
					}
					void (async () => {
						// One event carries both paired and Nearby invites; an unpaired
						// sender's belongs to `NearbyInviteDialog`, which shows the
						// fingerprint. Wait for `devices` to hydrate first, or a paired
						// sender briefly looks unpaired and gets misrouted.
						await pairingDataHydrated()
						if (disposed) return
						const { devices } = usePairingDataStore.getState()
						if (!isKnownPairedEndpoint(devices, payload.remote_endpoint_id)) {
							return
						}
						// A trusted device skips the dialog. The queue exists because
						// an invite can land mid-transfer, when nobody is here to
						// retry it — and because navigating the user away from
						// whatever they were doing would defeat unattended receiving.
						if (shouldAutoAccept(devices, payload.remote_endpoint_id)) {
							useAutoAcceptQueueStore.getState().enqueue(payload)
							return
						}
						setInvite(payload)
					})()
				}
			)
			if (disposed) {
				inviteUnlisten()
			} else {
				unlistenInvite = inviteUnlisten
			}

			const responseUnlisten = await listen(
				'paired-invite-response',
				(event: { payload: unknown }) => {
					try {
						const payload = JSON.parse(
							String(event.payload)
						) as PairedInviteResponsePayload
						if (payload.response !== 'declined') return
						const name =
							payload.display_name?.trim() ||
							t('common:sender.pairedDevices.unknownPeer')
						toastManager.add({
							title: t('common:sender.pairedDevices.inviteDeclined', {
								name,
							}),
							description: t('common:sender.pairedDevices.inviteDeclinedDesc'),
							type: 'warning',
						})
					} catch {
						// Ignore malformed response payloads
					}
				}
			)
			if (disposed) {
				responseUnlisten()
			} else {
				unlistenResponse = responseUnlisten
			}

			const expiredUnlisten = await listen('pairing-host-expired', () => {
				void refreshNodeStatus()
			})
			if (disposed) {
				expiredUnlisten()
			} else {
				unlistenExpired = expiredUnlisten
			}
		}

		void setup()

		return () => {
			disposed = true
			unlistenInvite?.()
			unlistenResponse?.()
			unlistenExpired?.()
		}
	}, [setInvite, refreshNodeStatus, t])

	return null
}
