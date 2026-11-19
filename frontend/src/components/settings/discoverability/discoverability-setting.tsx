import { useEffect, useState } from 'react'
import { Loader2 } from 'lucide-react'
import { useTranslation } from '@/i18n'
import { IS_NEARBY_CAPABLE } from '@/lib/platform'
import {
	getDiscoverability,
	setDiscoverability,
	type Discoverability,
} from '@/lib/pairing-api'
import { useAppSettingStore } from '@/store/app-setting'
import { Frame, FrameDescription, FramePanel, FrameTitle } from '../../ui/frame'
import { RadioGroup, RadioGroupItem } from '../../ui/radio-group'
import { toastManager } from '../../ui/toast'

const DISCOVERABILITY_OPTIONS = [
	{
		value: 'everyone' as const,
		labelKey: 'common:settings.devices.nearby.discoverabilityEveryone',
		descKey: 'common:settings.devices.nearby.discoverabilityEveryoneDesc',
	},
	{
		value: 'paired-only' as const,
		labelKey: 'common:settings.devices.nearby.discoverabilityPairedOnly',
		descKey: 'common:settings.devices.nearby.discoverabilityPairedOnlyDesc',
	},
	{
		value: 'off' as const,
		labelKey: 'common:settings.devices.nearby.discoverabilityOff',
		descKey: 'common:settings.devices.nearby.discoverabilityOffDesc',
	},
]

export function DiscoverabilitySetting() {
	const { t } = useTranslation()
	const [value, setValue] = useState<Discoverability>('everyone')
	const [isLoading, setIsLoading] = useState(true)
	const [isSaving, setIsSaving] = useState(false)

	useEffect(() => {
		if (!IS_NEARBY_CAPABLE) return

		let disposed = false

		void (async () => {
			try {
				const current = await getDiscoverability()
				if (!disposed) {
					setValue(current)
				}
			} catch (error) {
				console.error('Failed to get discoverability:', error)
				if (!disposed) {
					toastManager.add({
						title: t('common:settings.devices.nearby.discoverabilityTitle'),
						description: String(error),
						type: 'error',
					})
				}
			} finally {
				if (!disposed) {
					setIsLoading(false)
				}
			}
		})()

		return () => {
			disposed = true
		}
	}, [t])

	const handleChange = async (newValue: string) => {
		const nextValue = newValue as Discoverability
		if (nextValue === value) return
		setValue(nextValue)
		setIsSaving(true)

		try {
			await setDiscoverability(nextValue)
			// Persist only what the node accepted, so the next startup matches.
			useAppSettingStore.getState().setDiscoverability(nextValue)
		} catch (error) {
			console.error('Failed to set discoverability:', error)
			toastManager.add({
				title: t('common:settings.devices.nearby.discoverabilityTitle'),
				description: String(error),
				type: 'error',
			})
			// Revert to previous value on error
			try {
				const current = await getDiscoverability()
				setValue(current)
			} catch {
				// If we can't revert, just leave it as is
			}
		} finally {
			setIsSaving(false)
		}
	}

	if (!IS_NEARBY_CAPABLE) return null

	return (
		<Frame>
			<FramePanel className="flex flex-col gap-6">
				<div className="space-y-2">
					<FrameTitle>
						{t('common:settings.devices.nearby.discoverabilityTitle')}
					</FrameTitle>
					<FrameDescription>
						{t('common:settings.devices.nearby.discoverabilityDescription')}
					</FrameDescription>
				</div>

				{isLoading ? (
					<div className="flex items-center justify-center py-8">
						<Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
					</div>
				) : (
					<RadioGroup value={value} onValueChange={handleChange}>
						{/* The wrapping label makes the row one native activation target,
						    so `onValueChange` is the only trigger path. */}
						{DISCOVERABILITY_OPTIONS.map((option) => (
							<label
								key={option.value}
								htmlFor={`discoverability-${option.value}`}
								className="flex cursor-pointer items-start gap-3 text-left"
							>
								<RadioGroupItem
									id={`discoverability-${option.value}`}
									value={option.value}
									className="mt-0.5"
									disabled={isSaving}
								/>
								<div>
									<div className="text-sm font-medium">
										{t(option.labelKey)}
									</div>
									<div className="text-sm text-muted-foreground">
										{t(option.descKey)}
									</div>
								</div>
							</label>
						))}
					</RadioGroup>
				)}
			</FramePanel>
		</Frame>
	)
}
