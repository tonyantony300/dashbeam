import { Loader2 } from 'lucide-react'
import { useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { deviceTypeIcon } from '@/lib/device-icon'
import { getPairedSendCounts } from '@/lib/paired-send-counts'
import {
	deviceSubtitle,
	isPairedDeviceActive,
	matchesPairedDeviceSearch,
	type PairedDevice,
	sortPairedDevicesForList,
} from '@/lib/pairing-api'
import { IS_NEARBY_CAPABLE } from '@/lib/platform'
import { listen } from '@/lib/platform-api'
import { cn } from '@/lib/utils'
import {
	type NearbyDevice,
	startNearbyListeners,
	useNearbyStore,
} from '@/store/nearby-store'
import { useTranslation } from '../../i18n/react-i18next-compat'
import { DevicePairingStatus } from '../pairing/DevicePairingStatus'
import { PairedDevicesSearchField } from '../pairing/PairedDevicesSearchField'
import { Badge } from '../ui/badge'

type InviteStatus = 'sending' | 'sent' | 'failed'

interface PairedDevicesPanelProps {
	pairedDevices: PairedDevice[]
	pairedInviteStatus: Record<string, InviteStatus>
	isNodeReady: boolean
	isNodeStatusPending?: boolean
	hasTicket: boolean
	onInvitePairedDevice?: (endpointId: string) => Promise<boolean>
	onInviteNearbyDevice?: (endpointId: string) => Promise<boolean>
	onInviteSuccess?: () => void
	showHeader?: boolean
	showSearch?: boolean
	isOpen?: boolean
}

/**
 * Visible nearby rows before this block scrolls: 4 on phones, 3 on wider screens.
 * A row is 3.75rem (h-9 icon + py-3) plus the 1px divider between rows.
 */
const NEARBY_LIST_MAX_CLASS =
	'max-h-[calc(4*3.75rem+3px)] sm:max-h-[calc(3*3.75rem+2px)]'

function truncatedEndpointId(endpointId: string): string {
	return `${endpointId.slice(0, 8)}…`
}

export function PairedDevicesPanel({
	pairedDevices,
	pairedInviteStatus,
	isNodeReady,
	isNodeStatusPending = false,
	hasTicket,
	onInvitePairedDevice,
	onInviteNearbyDevice,
	onInviteSuccess,
	showHeader = true,
	showSearch = false,
	isOpen = false,
}: PairedDevicesPanelProps) {
	const { t } = useTranslation()
	const [searchQuery, setSearchQuery] = useState('')
	const nearbyDevices = useNearbyStore((s) => s.devices)
	const nearbyUnavailable = useNearbyStore((s) => s.unavailableReason)
	const hydrateNearby = useNearbyStore((s) => s.hydrate)

	useEffect(() => {
		if (isOpen) {
			setSearchQuery('')
		}
	}, [isOpen])

	useEffect(() => {
		if (!IS_NEARBY_CAPABLE || !isOpen) return

		let disposed = false
		let unlistenNearby: (() => void) | undefined
		let unlistenPaired: (() => void) | undefined

		void hydrateNearby()
		void startNearbyListeners().then((stop) => {
			if (disposed) {
				stop()
			} else {
				unlistenNearby = stop
			}
		})
		void listen('device-paired', () => void hydrateNearby()).then((stop) => {
			if (disposed) {
				stop()
			} else {
				unlistenPaired = stop
			}
		})

		return () => {
			disposed = true
			unlistenNearby?.()
			unlistenPaired?.()
		}
	}, [hydrateNearby, isOpen])

	const sortedDevices = useMemo(
		() => sortPairedDevicesForList(pairedDevices, getPairedSendCounts()),
		[pairedDevices]
	)

	const filteredDevices = useMemo(() => {
		if (!showSearch) return sortedDevices
		return sortedDevices.filter((device) =>
			matchesPairedDeviceSearch(device, searchQuery)
		)
	}, [sortedDevices, searchQuery, showSearch])

	const handleSelectPaired = async (endpointId: string) => {
		const success = await onInvitePairedDevice?.(endpointId)
		if (success) {
			onInviteSuccess?.()
		}
	}

	const handleSelectNearby = async (endpointId: string) => {
		const success = await onInviteNearbyDevice?.(endpointId)
		if (success) {
			onInviteSuccess?.()
		}
	}

	const anotherInviteInFlight = (endpointId: string) =>
		Object.entries(pairedInviteStatus).some(
			([id, status]) =>
				id !== endpointId && (status === 'sending' || status === 'sent')
		)

	const pairedListContent =
		sortedDevices.length === 0 ? (
			<div className="space-y-2 rounded-md border border-dashed px-3 py-6 text-center text-xs text-muted-foreground">
				<p className="font-medium text-foreground">
					{t('common:sender.sharingActive.devices.emptyTitle')}
				</p>
				<p>{t('common:sender.sharingActive.devices.emptyHint')}</p>
				<Link
					to="/settings/devices"
					className="inline-block underline underline-offset-2 hover:text-foreground"
				>
					{t('common:sender.sharingActive.devices.manageInSettings')}
				</Link>
			</div>
		) : filteredDevices.length === 0 ? (
			<p className="rounded-md border border-dashed px-3 py-6 text-center text-xs text-muted-foreground">
				{t('common:sender.sharingActive.devices.searchNoResults')}
			</p>
		) : (
			<ul className="divide-y divide-border">
				{filteredDevices.map((device) => {
					const Icon = deviceTypeIcon(device.device_type)
					const inviteStatus = pairedInviteStatus[device.endpoint_id]
					const isActive = isPairedDeviceActive(device)
					const isOnline = device.online
					const isSending = inviteStatus === 'sending'
					const disabled =
						!isNodeReady ||
						!hasTicket ||
						isSending ||
						!isActive ||
						!isOnline ||
						anotherInviteInFlight(device.endpoint_id) ||
						inviteStatus === 'sent'
					return (
						<li key={device.endpoint_id}>
							<button
								type="button"
								disabled={disabled}
								onClick={() => handleSelectPaired(device.endpoint_id)}
								aria-label={t('common:sender.pairedDevices.send')}
								className={cn(
									'flex w-full items-center justify-between gap-3 rounded-md px-3 py-3 text-left text-sm transition-colors',
									'hover:bg-muted/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
									'disabled:pointer-events-none disabled:opacity-50'
								)}
							>
								<div className="flex min-w-0 items-center gap-3">
									<div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground">
										{isSending ? (
											<Loader2 className="h-4 w-4 animate-spin" />
										) : (
											<Icon className="h-4 w-4" />
										)}
									</div>
									<div className="min-w-0">
										<span className="block truncate">
											{device.display_name}
										</span>
										<span className="block truncate text-xs text-muted-foreground">
											{isSending
												? t('common:sender.pairedDevices.sending')
												: deviceSubtitle(device)}
										</span>
									</div>
								</div>
								{!isSending ? (
									<DevicePairingStatus
										device={device}
										namespace="sender"
										className="shrink-0"
									/>
								) : null}
							</button>
						</li>
					)
				})}
			</ul>
		)

	const nearbyListContent = nearbyUnavailable ? (
		<div className="rounded-md border border-dashed px-3 py-4 text-center text-xs text-muted-foreground">
			<p className="font-medium text-foreground">
				{t('common:settings.devices.nearby.unavailableTitle')}
			</p>
			<p className="mt-1">{nearbyUnavailable}</p>
		</div>
	) : nearbyDevices.length === 0 ? (
		<p className="rounded-md border border-dashed px-3 py-4 text-center text-xs text-muted-foreground">
			{t('common:sender.sharingActive.devices.nearbyEmpty')}
		</p>
	) : (
		<ul
			className={cn(
				'divide-y divide-border overflow-y-auto',
				NEARBY_LIST_MAX_CLASS
			)}
		>
			{nearbyDevices.map((device) => (
				<NearbySheetRow
					key={device.endpointId}
					device={device}
					inviteStatus={pairedInviteStatus[device.endpointId]}
					disabled={
						!isNodeReady ||
						!hasTicket ||
						!onInviteNearbyDevice ||
						pairedInviteStatus[device.endpointId] === 'sending' ||
						pairedInviteStatus[device.endpointId] === 'sent' ||
						anotherInviteInFlight(device.endpointId)
					}
					onSelect={() => void handleSelectNearby(device.endpointId)}
				/>
			))}
		</ul>
	)

	const showNearbySection = IS_NEARBY_CAPABLE && Boolean(onInviteNearbyDevice)

	return (
		<div
			className={cn(
				showSearch ? 'flex h-full min-h-0 flex-col gap-2' : 'space-y-4'
			)}
		>
			{showHeader ? (
				<div className="space-y-0.5">
					<p className="text-sm font-medium">
						{t('common:sender.sharingActive.devices.title')}
					</p>
					<p className="text-xs text-muted-foreground">
						{t('common:sender.sharingActive.devices.hint')}
					</p>
				</div>
			) : null}

			{isNodeStatusPending ? (
				<p className="flex shrink-0 items-center gap-2 rounded-md border border-dashed px-3 py-3 text-xs text-muted-foreground">
					<Loader2 className="h-3.5 w-3.5 animate-spin" />
					{t('common:loading')}
				</p>
			) : !isNodeReady ? (
				<p className="shrink-0 rounded-md border border-dashed px-3 py-3 text-xs text-muted-foreground">
					{t('common:sender.sharingActive.devices.nodeUnavailable')}
				</p>
			) : null}

			<div
				className={cn(
					showSearch ? 'flex min-h-0 flex-1 flex-col gap-3' : 'space-y-4'
				)}
			>
				<section
					className={cn(
						'flex min-h-0 flex-col gap-2',
						showSearch && showNearbySection ? 'min-h-0 flex-1' : undefined
					)}
				>
					<p className="shrink-0 text-xs font-medium text-muted-foreground">
						{t('common:sender.sharingActive.devices.pairedSection')}
					</p>
					{showSearch && sortedDevices.length > 0 ? (
						<PairedDevicesSearchField
							value={searchQuery}
							onChange={setSearchQuery}
							className="shrink-0"
						/>
					) : null}
					{showSearch ? (
						<div className="min-h-0 flex-1 overflow-y-auto">
							{pairedListContent}
						</div>
					) : (
						pairedListContent
					)}
				</section>

				{showNearbySection ? (
					<section className="shrink-0 space-y-2">
						<p className="text-xs font-medium text-muted-foreground">
							{t('common:sender.sharingActive.devices.nearbySection')}
						</p>
						{nearbyListContent}
					</section>
				) : null}
			</div>
		</div>
	)
}

function NearbySheetRow({
	device,
	inviteStatus,
	disabled,
	onSelect,
}: {
	device: NearbyDevice
	inviteStatus: InviteStatus | undefined
	disabled: boolean
	onSelect: () => void
}) {
	const { t } = useTranslation()
	const Icon = deviceTypeIcon(device.deviceType)
	const isSending = inviteStatus === 'sending'
	const label =
		device.identified && device.displayName
			? device.displayName
			: truncatedEndpointId(device.endpointId)

	return (
		<li>
			<button
				type="button"
				disabled={disabled}
				onClick={onSelect}
				aria-label={t('common:sender.pairedDevices.send')}
				className={cn(
					'flex w-full items-center justify-between gap-3 rounded-md px-3 py-3 text-left text-sm transition-colors',
					'hover:bg-muted/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
					'disabled:pointer-events-none disabled:opacity-50'
				)}
			>
				<div className="flex min-w-0 items-center gap-3">
					<div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground">
						{isSending ? (
							<Loader2 className="h-4 w-4 animate-spin" />
						) : (
							<Icon className="h-4 w-4" />
						)}
					</div>
					<div className="flex min-w-0 items-center gap-1.5">
						<span className="truncate">{label}</span>
						{!isSending ? (
							<Badge variant="warning" size="sm" className="shrink-0">
								{t('common:settings.devices.nearby.unverified')}
							</Badge>
						) : null}
					</div>
				</div>
			</button>
		</li>
	)
}
