import { useEffect, useState } from 'react'
import { Loader2 } from 'lucide-react'
import { useTranslation } from '@/i18n'
import { deviceTypeIcon } from '@/lib/device-icon'
import {
	type Discoverability,
	formatDeviceTypeLabel,
	getDiscoverability,
	requestNearbyPair,
} from '@/lib/pairing-api'
import { IS_NEARBY_CAPABLE } from '@/lib/platform'
import { listen } from '@/lib/platform-api'
import {
	type NearbyDevice,
	startNearbyListeners,
	useNearbyStore,
} from '@/store/nearby-store'
import { useNearbyVerificationStore } from '@/store/nearby-verification-store'
import { Badge } from '../ui/badge'
import { Button } from '../ui/button'
import { Frame, FrameDescription, FramePanel, FrameTitle } from '../ui/frame'
import { toastManager } from '../ui/toast'

type PairState = 'pairing' | 'sent' | 'failed'

/** Fallback label for a device that hasn't answered the identity probe yet. */
function truncatedEndpointId(endpointId: string): string {
	return `${endpointId.slice(0, 8)}…`
}

/**
 * Nearby list for Settings → Devices. Pair sends a dedicated pairing request;
 * file sharing happens from the share sheet ("Send to a device").
 */
export function NearbyDevices() {
	const { t } = useTranslation()
	const devices = useNearbyStore((s) => s.devices)
	const showVerification = useNearbyVerificationStore((s) => s.show)
	const unavailableReason = useNearbyStore((s) => s.unavailableReason)
	const hydrate = useNearbyStore((s) => s.hydrate)
	const [discoverability, setDiscoverability] =
		useState<Discoverability | null>(null)
	const [pairState, setPairState] = useState<Record<string, PairState>>({})

	useEffect(() => {
		if (!IS_NEARBY_CAPABLE) return

		let disposed = false
		let unlistenNearby: (() => void) | undefined
		let unlistenPaired: (() => void) | undefined
		let unlistenResponse: (() => void) | undefined

		void hydrate()
		void getDiscoverability().then((value) => {
			if (!disposed) setDiscoverability(value)
		})
		void startNearbyListeners().then((stop) => {
			if (disposed) {
				stop()
			} else {
				unlistenNearby = stop
			}
		})
		void listen('device-paired', () => void hydrate()).then((stop) => {
			if (disposed) {
				stop()
			} else {
				unlistenPaired = stop
			}
		})
		void listen('paired-invite-response', (event: { payload: unknown }) => {
			let payload: {
				endpoint_id?: string
				response?: string
				display_name?: string | null
			}
			try {
				payload = JSON.parse(String(event.payload)) as typeof payload
			} catch {
				return
			}
			const endpointId = payload.endpoint_id
			if (!endpointId) return
			setPairState((prev) => {
				if (!(endpointId in prev)) return prev
				const next = { ...prev }
				delete next[endpointId]
				return next
			})
			if (payload.response === 'declined') {
				toastManager.add({
					title: t('common:settings.devices.nearby.pairDeclined', {
						name: payload.display_name || truncatedEndpointId(endpointId),
					}),
					type: 'info',
				})
			}
		}).then((stop) => {
			if (disposed) {
				stop()
			} else {
				unlistenResponse = stop
			}
		})

		return () => {
			disposed = true
			unlistenNearby?.()
			unlistenPaired?.()
			unlistenResponse?.()
		}
	}, [hydrate, t])

	if (!IS_NEARBY_CAPABLE) return null

	const handlePair = async (device: NearbyDevice) => {
		setPairState((prev) => ({ ...prev, [device.endpointId]: 'pairing' }))
		try {
			const delivered = await requestNearbyPair(device.endpointId)
			if (!delivered) {
				setPairState((prev) => ({ ...prev, [device.endpointId]: 'failed' }))
				toastManager.add({
					title: t('common:settings.devices.nearby.inviteFailed'),
					type: 'error',
				})
				return
			}
			setPairState((prev) => ({ ...prev, [device.endpointId]: 'sent' }))
			// The verification dialog already says the request is out.
			// Only once the request reached them — a code for a request that never
			// arrived is worse than none. Nearby devices are unpaired by
			// construction, so no extra check is needed.
			showVerification({
				endpointId: device.endpointId,
				name:
					device.identified && device.displayName
						? device.displayName
						: truncatedEndpointId(device.endpointId),
			})
		} catch (error) {
			console.error('Failed to request nearby pair:', error)
			setPairState((prev) => ({ ...prev, [device.endpointId]: 'failed' }))
			toastManager.add({
				title: t('common:settings.devices.nearby.inviteFailed'),
				type: 'error',
			})
		} finally {
			window.setTimeout(() => {
				setPairState((prev) => {
					const state = prev[device.endpointId]
					if (state !== 'failed' && state !== 'sent') return prev
					const next = { ...prev }
					delete next[device.endpointId]
					return next
				})
			}, 2000)
		}
	}

	const discoverabilityHint =
		discoverability === 'paired-only'
			? t('common:settings.devices.nearby.discoverabilityPairedOnlyHint')
			: discoverability === 'off'
				? t('common:settings.devices.nearby.discoverabilityOffHint')
				: null

	return (
		<Frame>
			<FramePanel className="flex flex-col gap-4">
				<div className="space-y-1">
					<FrameTitle>{t('common:settings.devices.nearby.heading')}</FrameTitle>
					<FrameDescription>
						{t('common:settings.devices.nearby.hint')}
					</FrameDescription>
					{discoverabilityHint ? (
						<p className="text-xs text-muted-foreground">
							{discoverabilityHint}
						</p>
					) : null}
				</div>

				{unavailableReason ? (
					<div className="rounded-md border border-dashed px-3 py-6 text-center text-xs text-muted-foreground">
						<p className="font-medium text-foreground">
							{t('common:settings.devices.nearby.unavailableTitle')}
						</p>
						<p className="mt-1">{unavailableReason}</p>
					</div>
				) : devices.length === 0 ? (
					<p className="rounded-md border border-dashed px-3 py-6 text-center text-xs text-muted-foreground">
						{t('common:settings.devices.nearby.empty')}
					</p>
				) : (
					<ul className="divide-y">
						{devices.map((device) => {
							const Icon = deviceTypeIcon(device.deviceType)
							const state = pairState[device.endpointId]
							const isPairing = state === 'pairing'
							const label =
								device.identified && device.displayName
									? device.displayName
									: truncatedEndpointId(device.endpointId)
							const typeLabel = formatDeviceTypeLabel(device.deviceType)

							return (
								<li
									key={device.endpointId}
									className="flex items-center justify-between gap-3 py-3 first:pt-0"
								>
									<div className="flex min-w-0 items-center gap-3">
										<div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground">
											{isPairing ? (
												<Loader2 className="h-4 w-4 animate-spin" />
											) : (
												<Icon className="h-4 w-4" />
											)}
										</div>
										<div className="min-w-0">
											<div className="flex min-w-0 items-center gap-1.5">
												<p className="truncate text-sm font-medium">{label}</p>
												<Badge variant="warning" size="sm" className="shrink-0">
													{t('common:settings.devices.nearby.unverified')}
												</Badge>
											</div>
											{typeLabel ? (
												<p className="truncate text-xs text-muted-foreground">
													{typeLabel}
												</p>
											) : null}
										</div>
									</div>
									<Button
										type="button"
										size="sm"
										variant="outline"
										className="shrink-0"
										disabled={isPairing}
										onClick={() => void handlePair(device)}
									>
										{isPairing
											? t('common:settings.devices.nearby.pairing')
											: t('common:settings.devices.nearby.pair')}
									</Button>
								</li>
							)
						})}
					</ul>
				)}
			</FramePanel>
		</Frame>
	)
}
