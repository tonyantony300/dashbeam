import type { DiscoveryConfigArg } from './discovery-config'
import { IS_PAIRING_CAPABLE } from './platform'
import { invoke } from './platform-api'
import type { RelayConfigArg } from './relay-config'

export interface DeviceInfo {
	endpoint_id: string
	display_name: string
	device_type: string
	os: string
}

export type PairingStatus =
	| 'active'
	| 'unpaired-remotely'
	| 'stale-local-identity'

export interface PairedDevice {
	endpoint_id: string
	display_name: string
	device_type: string
	os: string
	paired_at: number
	last_seen_at: number
	pairing_status: PairingStatus
	online: boolean
	trusted: boolean
}

export interface DevicePresencePayload {
	endpoint_id: string
	online: boolean
	last_seen_at: number
}

export interface DeviceUnpairedPayload {
	endpoint_id: string
	display_name?: string
	reason: 'local' | 'remote'
}

export interface PairedInvitePayload {
	blob_ticket: string
	file_count: number
	total_size: number
	sender_name: string
	remote_endpoint_id: string
}

export interface PairedInviteResponsePayload {
	endpoint_id: string
	display_name?: string | null
	response: 'accepted' | 'declined'
}

export interface InviteDelivered {
	delivered: boolean
}

export type NodeStatusKind = 'ready' | 'starting' | 'unavailable'

export interface NodeStatus {
	status: NodeStatusKind
	reason?: string | null
	/** True once the pairing node has warmed its relay/network path. */
	network_ready?: boolean
}

function pairingCapable(): boolean {
	return IS_PAIRING_CAPABLE
}

export async function getNodeStatus(): Promise<NodeStatus> {
	if (!pairingCapable()) {
		return {
			status: 'unavailable',
			reason: 'desktop_only',
			network_ready: false,
		}
	}
	return invoke<NodeStatus>('get_node_status')
}

export async function reconfigureNodeRelay(
	relay: RelayConfigArg,
	discovery: DiscoveryConfigArg
): Promise<void> {
	if (!pairingCapable()) return
	await invoke('reconfigure_node_relay', { relay, discovery })
}

export async function getDeviceInfo(): Promise<DeviceInfo | null> {
	if (!pairingCapable()) return null
	return invoke<DeviceInfo>('get_device_info')
}

export async function setDeviceDisplayName(
	displayName: string
): Promise<DeviceInfo | null> {
	if (!pairingCapable()) return null
	return invoke<DeviceInfo>('set_device_display_name', {
		displayName,
	})
}

export async function getPairingTicket(): Promise<string | null> {
	if (!pairingCapable()) return null
	return invoke<string>('get_pairing_ticket')
}

export async function startPairingHost(options?: {
	ttlSecs?: number | null
}): Promise<string> {
	if (!pairingCapable()) {
		throw new Error('Device pairing is not available on this platform')
	}
	const ttlSecs = options?.ttlSecs ?? null
	return invoke<string>('start_pairing_host', { ttlSecs })
}

export async function stopPairingHost(): Promise<void> {
	if (!pairingCapable()) return
	await invoke('stop_pairing_host')
}

export async function joinPairing(ticket: string): Promise<void> {
	if (!pairingCapable()) {
		throw new Error('Device pairing is not available on this platform')
	}
	await invoke('join_pairing', { ticket })
}

export async function listPairedDevices(): Promise<PairedDevice[]> {
	if (!pairingCapable()) return []
	return invoke<PairedDevice[]>('list_paired_devices')
}

export async function forgetPairedDevice(endpointId: string): Promise<void> {
	if (!pairingCapable()) return
	await invoke('forget_paired_device', { endpointId })
}

export async function renamePairedDevice(
	endpointId: string,
	displayName: string
): Promise<PairedDevice | null> {
	if (!pairingCapable()) return null
	return invoke<PairedDevice>('rename_paired_device', {
		endpointId,
		displayName,
	})
}

/**
 * IPC seam: `endpointId` / `trust` must match the parameter names of
 * `trust_paired_device` in `src-tauri/src/commands.rs`. Tauri maps payload keys
 * to parameter names, so renaming one side alone fails at runtime.
 */
export async function trustPairedDevice(
	endpointId: string,
	trust: boolean
): Promise<PairedDevice | null> {
	if (!pairingCapable()) return null
	return invoke<PairedDevice>('trust_paired_device', { endpointId, trust })
}

export async function invitePairedDevice(
	endpointId: string,
	blobTicket: string,
	fileCount: number,
	totalSize: number
): Promise<boolean> {
	if (!pairingCapable()) return false
	const result = await invoke<InviteDelivered>('invite_paired_device', {
		endpointId,
		blobTicket,
		fileCount,
		totalSize,
	})
	return result.delivered
}

export async function respondPairedInvite(
	endpointId: string,
	accepted: boolean
): Promise<void> {
	if (!pairingCapable()) return
	await invoke('respond_paired_invite', {
		endpointId,
		accepted,
	})
}

export type Discoverability = 'everyone' | 'paired-only' | 'off'

export async function getDiscoverability(): Promise<Discoverability> {
	if (!pairingCapable()) return 'off'
	return invoke<Discoverability>('get_discoverability')
}

/**
 * IPC seam: the key must match the `setting` parameter of
 * `set_discoverability` in `src-tauri/src/commands.rs`. Tauri matches payload
 * keys to parameter names, so renaming one side alone fails at runtime.
 */
type SetDiscoverabilityArgs = {
	setting: Discoverability
}

export async function setDiscoverability(
	value: Discoverability
): Promise<void> {
	if (!pairingCapable()) return
	const args: SetDiscoverabilityArgs = { setting: value }
	await invoke('set_discoverability', args)
}

export interface NearbyStatus {
	/**
	 * Why LAN discovery is unavailable, or null when running or off. Queryable
	 * because `nearby-unavailable` can fire during node init, before any
	 * frontend listener exists.
	 */
	reason: string | null
}

export async function getNearbyStatus(): Promise<NearbyStatus> {
	if (!pairingCapable()) return { reason: null }
	return invoke<NearbyStatus>('nearby_status')
}

/**
 * Delivers the active share ticket to a Nearby (unpaired LAN) device.
 * Same ticket reuse as `invitePairedDevice` — no separate share is minted.
 */
export async function inviteNearbyDevice(
	endpointId: string,
	blobTicket: string,
	fileCount: number,
	totalSize: number
): Promise<boolean> {
	if (!pairingCapable()) return false
	const result = await invoke<InviteDelivered>('invite_nearby_device', {
		endpointId,
		blobTicket,
		fileCount,
		totalSize,
	})
	return result.delivered
}

/** Asks a Nearby LAN device to pair — no file ticket. */
export async function requestNearbyPair(endpointId: string): Promise<boolean> {
	if (!pairingCapable()) return false
	const result = await invoke<InviteDelivered>('request_nearby_pair', {
		endpointId,
	})
	return result.delivered
}

export async function respondNearbyInvite(
	endpointId: string,
	accept: boolean,
	block = false
): Promise<void> {
	if (!pairingCapable()) return
	await invoke('respond_nearby_invite', { endpointId, accept, block })
}

export function formatOsLabel(os: string | undefined | null): string {
	switch ((os ?? '').toLowerCase()) {
		case 'macos':
			return 'macOS'
		case 'windows':
			return 'Windows'
		case 'linux':
			return 'Linux'
		case 'ios':
			return 'iOS'
		case 'android':
			return 'Android'
		default:
			return os?.trim() || ''
	}
}

export function formatDeviceTypeLabel(
	deviceType: string | undefined | null
): string {
	switch ((deviceType ?? '').toLowerCase()) {
		case 'laptop':
			return 'Laptop'
		case 'desktop':
			return 'Desktop'
		case 'phone':
			return 'Phone'
		case 'tablet':
			return 'Tablet'
		case 'agent':
			return 'Agent'
		default:
			return deviceType?.trim() || 'Device'
	}
}

export function deviceSubtitle(
	device:
		| Pick<PairedDevice, 'device_type' | 'os'>
		| Pick<DeviceInfo, 'device_type' | 'os'>
): string {
	const typeLabel = formatDeviceTypeLabel(device.device_type)
	const osLabel = formatOsLabel(device.os)
	return osLabel ? `${typeLabel} ${osLabel}` : typeLabel
}

export function matchesPairedDeviceSearch(
	device: PairedDevice,
	query: string
): boolean {
	const normalizedQuery = query.trim().toLowerCase()
	if (!normalizedQuery) return true

	const searchable = [
		device.display_name,
		device.device_type,
		device.os,
		formatDeviceTypeLabel(device.device_type),
		formatOsLabel(device.os),
		deviceSubtitle(device),
		device.online ? 'online' : 'offline',
		isPairedDeviceActive(device) ? 'active' : 'unpaired',
	]
		.join(' ')
		.toLowerCase()

	return searchable.includes(normalizedQuery)
}

export function isPairedDeviceActive(
	device: Pick<PairedDevice, 'pairing_status'>
): boolean {
	return (device.pairing_status ?? 'active') === 'active'
}

/**
 * Whether this device's shares are accepted without prompting.
 *
 * Reads the stored flag only — routing an incoming invite must also check that
 * the pairing is still active, which `shouldAutoAccept` in `lib/auto-accept.ts`
 * does.
 */
export function isTrustedDevice(
	device: Pick<PairedDevice, 'trusted'>
): boolean {
	return device.trusted === true
}

/**
 * True when `endpointId` already has a paired-device record — routes an invite
 * to the paired dialog rather than the Nearby fingerprint one.
 */
export function isKnownPairedEndpoint(
	devices: Pick<PairedDevice, 'endpoint_id'>[],
	endpointId: string
): boolean {
	const id = endpointId.toLowerCase()
	return devices.some((device) => device.endpoint_id.toLowerCase() === id)
}

/** 0 = online+active, 1 = offline+active, 2 = unpaired */
export function pairedDeviceListRank(
	device: Pick<PairedDevice, 'online' | 'pairing_status'>
): number {
	if (!isPairedDeviceActive(device)) return 2
	if (!device.online) return 1
	return 0
}

export function comparePairedDevicesForList(
	a: PairedDevice,
	b: PairedDevice,
	sendCounts: Record<string, number> = {}
): number {
	const rankA = pairedDeviceListRank(a)
	const rankB = pairedDeviceListRank(b)
	if (rankA !== rankB) return rankA - rankB

	// Online devices: most-sent first, then name.
	if (rankA === 0) {
		const countA = sendCounts[a.endpoint_id.toLowerCase()] ?? 0
		const countB = sendCounts[b.endpoint_id.toLowerCase()] ?? 0
		if (countA !== countB) return countB - countA
		return a.display_name.localeCompare(b.display_name)
	}

	// Offline / unpaired: most recently seen first so a device that just went
	// offline lands immediately after the last online device.
	const seenDiff = (b.last_seen_at ?? 0) - (a.last_seen_at ?? 0)
	if (seenDiff !== 0) return seenDiff
	return a.display_name.localeCompare(b.display_name)
}

export function sortPairedDevicesForList(
	devices: PairedDevice[],
	sendCounts: Record<string, number> = {}
): PairedDevice[] {
	return [...devices].sort((a, b) =>
		comparePairedDevicesForList(a, b, sendCounts)
	)
}

/** Settings list: most-sent devices first (local send-click counts), then name. */
export function sortPairedDevicesForSettings(
	devices: PairedDevice[],
	sendCounts: Record<string, number>
): PairedDevice[] {
	return [...devices].sort((a, b) => {
		const countA = sendCounts[a.endpoint_id.toLowerCase()] ?? 0
		const countB = sendCounts[b.endpoint_id.toLowerCase()] ?? 0
		if (countA !== countB) return countB - countA
		return a.display_name.localeCompare(b.display_name)
	})
}

export function patchDevicePresence(
	devices: PairedDevice[],
	payload: DevicePresencePayload
): PairedDevice[] {
	const id = payload.endpoint_id.toLowerCase()
	return devices.map((device) =>
		device.endpoint_id.toLowerCase() === id
			? {
					...device,
					online: payload.online,
					last_seen_at: payload.last_seen_at,
				}
			: device
	)
}
