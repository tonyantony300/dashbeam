import type React from 'react'
import { type ReactNode, useEffect, useState } from 'react'
import { TranslationContext } from './context'
import i18next, { getInitialLanguage, loadTranslations } from './setup'

export const TranslationProvider: React.FC<{ children: ReactNode }> = ({
	children,
}) => {
	const [language, setLanguage] = useState(i18next.language)

	useEffect(() => {
		try {
			loadTranslations()
		} catch (_error) {}
	}, [])

	useEffect(() => {
		const handleLanguageChange = () => {
			const newLang = getInitialLanguage()
			if (newLang !== language) {
				setLanguage(newLang)
				i18next.changeLanguage(newLang)
			}
		}

		const handleStorageChange = (e: StorageEvent) => {
			if (e.key === 'altsendme-language') {
				handleLanguageChange()
			}
		}

		window.addEventListener('languagechange', handleLanguageChange)
		window.addEventListener('storage', handleStorageChange)

		return () => {
			window.removeEventListener('languagechange', handleLanguageChange)
			window.removeEventListener('storage', handleStorageChange)
		}
	}, [language])

	return (
		<TranslationContext.Provider
			value={{
				t: i18next.t,
				i18n: i18next,
			}}
		>
			{children}
		</TranslationContext.Provider>
	)
}

export default TranslationProvider
