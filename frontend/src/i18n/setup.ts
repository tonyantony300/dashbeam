import { resolveLanguage } from '../lib/language-detection'

export interface TranslationResources {
	[language: string]: {
		[namespace: string]: {
			[key: string]: string
		}
	}
}

export interface I18nInstance {
	language: string
	fallbackLng: string
	resources: TranslationResources
	namespaces: string[]
	defaultNS: string
	changeLanguage: (lng: string) => void
	t: (key: string, options?: Record<string, unknown>) => string
}

let i18nInstance: I18nInstance

const localeFiles = import.meta.glob('../locales/**/*.json', { eager: true })

const resources: TranslationResources = {}
const namespaces: string[] = []

Object.entries(localeFiles).forEach(([path, module]) => {
	const match = path.match(/\.\.\/locales\/([^/]+)\/([^/]+)\.json/)

	if (match) {
		const [, language, namespace] = match

		if (!resources[language]) {
			resources[language] = {}
		}

		if (!namespaces.includes(namespace)) {
			namespaces.push(namespace)
		}

		resources[language][namespace] =
			(module as { default: { [key: string]: string } }).default ||
			(module as { [key: string]: string })
	}
})

const getStoredLanguage = (): string | null => {
	try {
		return localStorage.getItem('altsendme-language')
	} catch {
		return null
	}
}

const getSystemLanguages = (): readonly string[] => {
	if (typeof navigator === 'undefined') {
		return []
	}
	return navigator.languages?.length
		? navigator.languages
		: [navigator.language].filter(Boolean)
}

export const getInitialLanguage = (): string =>
	resolveLanguage({
		stored: getStoredLanguage(),
		preferred: getSystemLanguages(),
		available: Object.keys(resources),
		fallback: 'en',
	})

const translate = (
	key: string,
	options: Record<string, unknown> = {}
): string => {
	const { language, fallbackLng, resources: res, defaultNS } = i18nInstance

	let namespace = defaultNS
	let translationKey = key

	if (key.includes(':')) {
		const parts = key.split(':')
		namespace = parts[0]
		translationKey = parts[1]
	}

	const getNestedValue = (
		obj: Record<string, unknown>,
		path: string
	): string | undefined => {
		return path.split('.').reduce((current, key) => {
			return current &&
				typeof current === 'object' &&
				current !== null &&
				key in current
				? (current as Record<string, unknown>)[key]
				: undefined
		}, obj as unknown) as string | undefined
	}

	let translation = getNestedValue(res[language]?.[namespace], translationKey)

	if (translation === undefined && language !== fallbackLng) {
		translation = getNestedValue(res[fallbackLng]?.[namespace], translationKey)
	}

	if (translation === undefined) {
		return key
	}

	if (typeof translation === 'string' && options) {
		return translation.replace(/\{\{(\w+)\}\}/g, (match, variable) => {
			return options[variable] !== undefined ? String(options[variable]) : match
		})
	}

	return String(translation)
}

const changeLanguage = (lng: string): void => {
	if (i18nInstance && resources[lng]) {
		i18nInstance.language = lng

		try {
			localStorage.setItem('altsendme-language', lng)
		} catch (_error) {}
	}
}

const initI18n = (): I18nInstance => {
	const currentLanguage = getInitialLanguage()

	i18nInstance = {
		language: currentLanguage,
		fallbackLng: 'en',
		resources,
		namespaces,
		defaultNS: 'common',
		changeLanguage,
		t: translate,
	}

	return i18nInstance
}

export const loadTranslations = (): void => {}

const i18n = initI18n()

export default i18n
