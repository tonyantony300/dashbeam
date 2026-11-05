import type { LucideIcon } from 'lucide-react'
import { Bot, Laptop, Monitor, Smartphone, Tablet } from 'lucide-react'

export function deviceTypeIcon(
	deviceType: string | undefined | null
): LucideIcon {
	switch ((deviceType ?? '').toLowerCase()) {
		case 'laptop':
			return Laptop
		case 'desktop':
			return Monitor
		case 'tablet':
			return Tablet
		case 'phone':
			return Smartphone
		case 'agent':
			return Bot
		default:
			return Monitor
	}
}
