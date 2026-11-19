import { useEffect, useState } from 'react'
import { useLocation, useNavigate } from 'react-router-dom'
import { useTranslation } from '@/i18n'
import { IS_NEARBY_CAPABLE } from '@/lib/platform'
import { listen } from '@/lib/platform-api'
import { formatFileSize } from '@/lib/utils'
import { shortFingerprint } from '@/lib/fingerprint'
import {
	isKnownPairedEndpoint,
	respondNearbyInvite,
	type PairedInvitePayload,
} from '@/lib/pairing-api'
import {
	pairingDataHydrated,
	usePairingDataStore,
} from '@/store/pairing-data-store'
import { useReceiverActionsStore } from '@/store/receiver-actions-store'
import {
	AlertDialog,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
	AlertDialogClose,
} from '../ui/alert-dialog'
import { Button } from '../ui/button'
import { toastManager } from '../ui/toast'
import { VerificationCode } from './VerificationCode'

/**
 * The receiver's prompt for an invite from a device that isn't paired yet.
 * `paired-invite-received` carries both paired and Nearby senders, so this only
 * reacts when the sender is not already known — the routine case goes to
 * `PairedInviteDialog`.
 */
export function NearbyInviteDialog() {
	const { t } = useTranslation()
	const navigate = useNavigate()
	const location = useLocation()
	const [invite, setInvite] = useState<PairedInvitePayload | null>(null)
	const acceptPairedInvite = useReceiverActionsStore(
		(s) => s.acceptPairedInvite
	)

	useEffect(() => {
		if (!IS_NEARBY_CAPABLE) return

		let disposed = false
		let unlisten: (() => void) | undefined

		const setup = async () => {
			const stop = await listen(
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
						// Wait for `devices` to hydrate, or a paired sender's invite
						// shows here as "unverified" instead of in `PairedInviteDialog`.
						await pairingDataHydrated()
						if (disposed) return
						const { devices } = usePairingDataStore.getState()
						if (isKnownPairedEndpoint(devices, payload.remote_endpoint_id)) {
							return
						}
						setInvite(payload)
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

	const notifyInviteResponse = (endpointId: string, accepted: boolean) => {
		void respondNearbyInvite(endpointId, accepted).catch(() => {
			// Best-effort notify; accept/decline UI already proceeded.
		})
	}

	const decline = () => {
		// One close click fires both `AlertDialogClose`'s `onClick` and
		// `onOpenChange`, so read-and-clear atomically via the state updater and
		// the second call becomes a no-op instead of a second decline.
		setInvite((current) => {
			if (!current) return current
			notifyInviteResponse(current.remote_endpoint_id, false)
			return null
		})
	}

	const accept = async () => {
		const current = invite
		if (!current) return
		// Never accept without a verification code (malformed endpoint id). The
		// button below is disabled for the same reason.
		if (!shortFingerprint(current.remote_endpoint_id)) return
		if (!acceptPairedInvite) {
			toastManager.add({
				title: t('common:errors.receiveFailed'),
				description: t('common:receiver.openReceiveTabHint'),
				type: 'warning',
			})
			return
		}

		setInvite(null)
		notifyInviteResponse(current.remote_endpoint_id, true)
		if (location.pathname !== '/') {
			navigate('/')
		}
		try {
			await acceptPairedInvite(current)
		} catch {
			// receiveWithTicket / accept path surfaces its own errors
		}
	}

	const fingerprint = invite
		? shortFingerprint(invite.remote_endpoint_id)
		: null
	const canAccept = invite != null && fingerprint != null

	return (
		<AlertDialog
			open={invite != null}
			onOpenChange={(open) => {
				if (!open) decline()
			}}
		>
			<AlertDialogContent>
				<AlertDialogHeader>
					<AlertDialogTitle>
						{t('common:receiver.receiveFromNearbyTitle')}
					</AlertDialogTitle>
					<AlertDialogDescription>
						{invite
							? invite.total_size > 0
								? t('common:receiver.receiveFromNearbyDescription', {
										sender: invite.sender_name,
										count: invite.file_count,
										size: formatFileSize(invite.total_size),
									})
								: t('common:receiver.receiveFromNearbyDescriptionNoSize', {
										sender: invite.sender_name,
										count: invite.file_count,
									})
							: ''}
					</AlertDialogDescription>
				</AlertDialogHeader>
				<div className="space-y-2 px-6 pb-4">
					<p className="text-xs font-medium text-muted-foreground">
						{t('common:receiver.nearbyInvite.unverifiedSender')}
					</p>
					{invite ? (
						<VerificationCode
							code={fingerprint}
							hint={t('common:receiver.nearbyInvite.fingerprintHint', {
								sender: invite.sender_name,
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
					<Button size="sm" disabled={!canAccept} onClick={() => void accept()}>
						{t('common:receiver.acceptInvite')}
					</Button>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	)
}
