import assert from 'node:assert/strict'
import { readdirSync } from 'node:fs'
import { describe, it } from 'node:test'
import { matchLanguage, resolveLanguage } from './language-detection.js'

// Read from the locales folder so newly added translations are covered too.
const AVAILABLE = readdirSync('frontend/src/locales', { withFileTypes: true })
	.filter((entry) => entry.isDirectory())
	.map((entry) => entry.name)

const resolve = (preferred: string[], stored: string | null = null) =>
	resolveLanguage({ stored, preferred, available: AVAILABLE, fallback: 'en' })

describe('resolveLanguage', () => {
	it('uses the system language when nothing is saved', () => {
		assert.equal(resolve(['pt-BR', 'pt']), 'pt-BR')
		assert.equal(resolve(['ja-JP']), 'ja')
	})

	it('keeps an explicit saved choice over the system language', () => {
		assert.equal(resolve(['pt-BR'], 'de'), 'de')
		assert.equal(resolve(['pt-BR'], 'en'), 'en')
	})

	it('ignores a saved value that is not a shipped locale', () => {
		assert.equal(resolve(['fr-FR'], 'xx'), 'fr')
	})

	it('picks the first supported language in preference order', () => {
		assert.equal(resolve(['nl-NL', 'fr-FR', 'de-DE']), 'fr')
	})

	it('falls back to English when no preference is supported', () => {
		assert.equal(resolve(['nl-NL', 'vi-VN']), 'en')
		assert.equal(resolve([]), 'en')
	})
})

describe('matchLanguage', () => {
	it('resolves every shipped locale to itself, ignoring case', () => {
		assert.ok(AVAILABLE.length > 1)
		for (const code of AVAILABLE) {
			assert.equal(matchLanguage(code, AVAILABLE), code)
			assert.equal(matchLanguage(code.toUpperCase(), AVAILABLE), code)
		}
	})

	it('maps regional variants to the shipped language', () => {
		assert.equal(matchLanguage('pt-PT', AVAILABLE), 'pt-BR')
		assert.equal(matchLanguage('pt', AVAILABLE), 'pt-BR')
		assert.equal(matchLanguage('de-AT', AVAILABLE), 'de')
		assert.equal(matchLanguage('es-419', AVAILABLE), 'es')
		assert.equal(matchLanguage('uz-UZ', AVAILABLE), 'uz-Latn')
		assert.equal(matchLanguage('en_GB', AVAILABLE), 'en')
	})

	it('maps Norwegian Bokmål and Nynorsk to the Norwegian translation', () => {
		assert.equal(matchLanguage('nb-NO', AVAILABLE), 'no')
		assert.equal(matchLanguage('nn', AVAILABLE), 'no')
	})

	it('separates Traditional and Simplified Chinese', () => {
		for (const tag of ['zh-HK', 'zh-MO', 'zh-Hant', 'zh-Hant-HK']) {
			assert.equal(matchLanguage(tag, AVAILABLE), 'zh-TW', tag)
		}
		for (const tag of ['zh', 'zh-SG', 'zh-Hans', 'zh-Hans-TW']) {
			assert.equal(matchLanguage(tag, AVAILABLE), 'zh-CN', tag)
		}
	})

	it('returns undefined for languages without a translation', () => {
		assert.equal(matchLanguage('nl-NL', AVAILABLE), undefined)
		assert.equal(matchLanguage('', AVAILABLE), undefined)
	})
})
