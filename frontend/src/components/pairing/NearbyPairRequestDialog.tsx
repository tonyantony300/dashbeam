import { useEffect, useState } from 'react'
import { useTranslation } from '@/i18n'
import {
	formatDeviceTypeLabel,
	isKnownPairedEndpoint,
	respondNearbyInvite,
} from '@/lib/pairing-api'
import { IS_NEARBY_CAPABLE } from '@/lib/platform'
import { shortFingerprint } from '@/lib/fingerprint'
import { listen } from '@/lib/platform-api'
import {
	pairingDataHydrated,
	usePairingDataStore,
} from '@/store/pairing-data-store'
import {
	AlertDialog,
	AlertDialogClose,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from '../ui/alert-dialog'
import { Button } from '../ui/button'
import { VerificationCode } from './VerificationCode'

export type NearbyPairRequestPayload = {
	remote_endpoint_id: string
	sender_name: string
	device_type: string
	os?: string
}

/**
 * Receiver prompt for a dedicated nearby pairing request (no files).
 * Accept only records the peer as paired via `respond_nearby_invite`.
 */
export function NearbyPairRequestDialog() {
	const { t } = useTranslation()
	const [request, setRequest] = useState<NearbyPairRequestPayload | null>(null)

	useEffect(() => {
		if (!IS_NEARBY_CAPABLE) return

		let disposed = false
		let unlisten: (() => void) | undefined

		const setup = async () => {
			const stop = await listen(
				'nearby-pair-request-received',
				(event: { payload: unknown }) => {
					let payload: NearbyPairRequestPayload
					try {
						payload = JSON.parse(
							String(event.payload)
						) as NearbyPairRequestPayload
					} catch {
						return
					}
					void (async () => {
						await pairingDataHydrated()
						if (disposed) return
						const { devices } = usePairingDataStore.getState()
						if (isKnownPairedEndpoint(devices, payload.remote_endpoint_id)) {
							return
						}
						setRequest(payload)
					})()
				}
			)
			if (disposed) {
				stop()
			} else {
				unlisten = stop
			}
		}

		void setup()

		return () => {
			disposed = true
			unlisten?.()
		}
	}, [])

	if (!IS_NEARBY_CAPABLE) return null

	const notify = (endpointId: string, accepted: boolean) => {
		void respondNearbyInvite(endpointId, accepted).catch(() => {
			// Best-effort; UI already proceeded.
		})
	}

	const decline = () => {
		setRequest((current) => {
			if (!current) return current
			notify(current.remote_endpoint_id, false)
			return null
		})
	}

	const accept = () => {
		const current = request
		if (!current) return
		setRequest(null)
		notify(current.remote_endpoint_id, true)
	}

	const typeLabel = request ? formatDeviceTypeLabel(request.device_type) : ''
	// Pairing grants persistent access, so it gets at least the confirmation a
	// one-off file invite does. Display only — Accept stays enabled.
	const fingerprint = request
		? shortFingerprint(request.remote_endpoint_id)
		: null

	return (
		<AlertDialog
			open={request != null}
			onOpenChange={(open) => {
				if (!open) decline()
			}}
		>
			<AlertDialogContent>
				<AlertDialogHeader>
					<AlertDialogTitle>
						{t('common:receiver.nearbyPairRequest.title')}
					</AlertDialogTitle>
					<AlertDialogDescription>
						{request
							? typeLabel
								? t('common:receiver.nearbyPairRequest.descriptionWithType', {
										sender: request.sender_name,
										deviceType: typeLabel,
									})
								: t('common:receiver.nearbyPairRequest.description', {
										sender: request.sender_name,
									})
							: ''}
					</AlertDialogDescription>
				</AlertDialogHeader>
				<div className="px-6 pb-4">
					{/* Guarded on `request`: the popup stays mounted through its close
					    animation, so an unguarded render flashes the "no code" error
					    for that frame once Accept has cleared the request. */}
					{request ? (
						<VerificationCode
							code={fingerprint}
							hint={t('common:receiver.nearbyInvite.fingerprintHint', {
								sender: request.sender_name,
							})}
							unavailable={t(
								'common:receiver.nearbyInvite.fingerprintUnavailable'
							)}
						/>
					) : null}
				</div>
				<AlertDialogFooter>
					<AlertDialogClose
						render={
							<Button size="sm" variant="outline">
								{t('common:receiver.declineInvite')}
							</Button>
						}
						onClick={decline}
					/>
					<Button size="sm" onClick={accept}>
						{t('common:receiver.acceptInvite')}
					</Button>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	)
}
