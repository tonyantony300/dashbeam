/**
 * Picks the UI language: an explicit saved choice wins, then the first
 * OS/browser preference with a shipped translation, then the fallback.
 * The detected language is not saved, so the app keeps following the
 * system language until the user picks one.
 */
export function resolveLanguage(input: {
	stored: string | null
	preferred: readonly string[]
	available: readonly string[]
	fallback: string
}): string {
	const { stored, preferred, available, fallback } = input

	if (stored && available.includes(stored)) {
		return stored
	}

	for (const tag of preferred) {
		const match = matchLanguage(tag, available)
		if (match) {
			return match
		}
	}

	return fallback
}

const TRADITIONAL_CHINESE_REGIONS = new Set(['tw', 'hk', 'mo'])

/**
 * Maps a BCP 47 tag such as `pt-PT`, `nb-NO` or `zh-HK` to a shipped locale.
 */
export function matchLanguage(
	tag: string,
	available: readonly string[]
): string | undefined {
	const subtags = tag
		.trim()
		.replace(/_/g, '-')
		.toLowerCase()
		.split('-')
		.filter(Boolean)

	if (subtags.length === 0) {
		return undefined
	}

	const byLowerCase = new Map(
		available.map((code) => [code.toLowerCase(), code])
	)
	const exact = byLowerCase.get(subtags.join('-'))
	if (exact) {
		return exact
	}

	let base = subtags[0]
	// Browsers report Norwegian as Bokmål (nb) or Nynorsk (nn).
	if (base === 'nb' || base === 'nn') {
		base = 'no'
	}

	const sameBase = available.find(
		(code) => code.toLowerCase().split('-')[0] === base
	)

	if (base === 'zh') {
		const rest = subtags.slice(1)
		const traditional =
			rest.includes('hant') ||
			(!rest.includes('hans') &&
				rest.some((subtag) => TRADITIONAL_CHINESE_REGIONS.has(subtag)))
		return byLowerCase.get(traditional ? 'zh-tw' : 'zh-cn') ?? sameBase
	}

	return byLowerCase.get(base) ?? sameBase
}
