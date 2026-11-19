/**
 * Release notes for the "What's new" dialog: a deliberately small markdown
 * subset (what the hand-written GitHub release bodies actually use), parsed to
 * plain data so nothing from the network is ever injected as HTML.
 */

export type InlineNode =
	| { type: 'text'; text: string }
	| { type: 'code'; text: string }
	| { type: 'bold'; children: InlineNode[] }
	| { type: 'link'; text: string; href: string }

export type NotesBlock =
	| { type: 'heading'; level: number; inline: InlineNode[] }
	| { type: 'paragraph'; inline: InlineNode[] }
	| { type: 'list'; ordered: boolean; items: InlineNode[][] }
	| { type: 'rule' }

// Bold, inline code, [text](url), bare URL. Only http(s) ever becomes a link.
const INLINE =
	/\*\*(.+?)\*\*|`([^`]+)`|\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)|(https?:\/\/[^\s<>)\]]+)/g

export function parseInline(source: string): InlineNode[] {
	const nodes: InlineNode[] = []
	const pushText = (text: string) => {
		if (!text) return
		const last = nodes[nodes.length - 1]
		if (last?.type === 'text') last.text += text
		else nodes.push({ type: 'text', text })
	}

	let cursor = 0
	for (const match of source.matchAll(INLINE)) {
		pushText(source.slice(cursor, match.index))
		cursor = match.index + match[0].length
		const [, bold, code, linkText, linkHref, bareUrl] = match
		if (bold !== undefined) {
			nodes.push({ type: 'bold', children: parseInline(bold) })
		} else if (code !== undefined) {
			nodes.push({ type: 'code', text: code })
		} else if (linkHref !== undefined) {
			nodes.push({ type: 'link', text: linkText, href: linkHref })
		} else {
			// Sentence punctuation after a bare URL is not part of it.
			const href = bareUrl.replace(/[.,;:!?]+$/, '')
			nodes.push({ type: 'link', text: href, href })
			pushText(bareUrl.slice(href.length))
		}
	}
	pushText(source.slice(cursor))
	return nodes
}

const HEADING = /^(#{1,6})\s+(.*?)\s*#*$/
const BULLET = /^\s*[-*+]\s+(.*)$/
const NUMBERED = /^\s*\d+[.)]\s+(.*)$/
const RULE = /^\s*([-*_])(\s*\1){2,}\s*$/

export function parseReleaseNotes(markdown: string): NotesBlock[] {
	const blocks: NotesBlock[] = []
	let paragraph: string[] = []
	let list: { ordered: boolean; items: string[] } | null = null

	const flush = () => {
		if (paragraph.length > 0) {
			blocks.push({
				type: 'paragraph',
				inline: parseInline(paragraph.join(' ')),
			})
			paragraph = []
		}
		if (list) {
			blocks.push({
				type: 'list',
				ordered: list.ordered,
				items: list.items.map(parseInline),
			})
			list = null
		}
	}

	for (const rawLine of markdown.replace(/\r\n?/g, '\n').split('\n')) {
		const line = rawLine.trimEnd()
		if (line.trim() === '') {
			flush()
			continue
		}
		if (RULE.test(line)) {
			flush()
			blocks.push({ type: 'rule' })
			continue
		}
		const heading = line.match(HEADING)
		if (heading) {
			flush()
			blocks.push({
				type: 'heading',
				level: heading[1].length,
				inline: parseInline(heading[2]),
			})
			continue
		}
		const bullet = line.match(BULLET)
		const numbered = bullet ? null : line.match(NUMBERED)
		const item = bullet ?? numbered
		if (item) {
			const ordered = numbered !== null
			// Nested lists flatten into their parent; a change of kind starts anew.
			if (!list || list.ordered !== ordered) {
				flush()
				list = { ordered, items: [] }
			}
			list.items.push(item[1])
			continue
		}
		// An indented line under a list item continues that item.
		if (list && /^\s+/.test(line)) {
			list.items[list.items.length - 1] += ` ${line.trim()}`
			continue
		}
		if (list) flush()
		paragraph.push(line.trim())
	}
	flush()

	return dropReleasePageFooter(blocks)
}

/**
 * Release bodies end with "--- / See the assets below…", which only makes
 * sense on the release page. A final rule followed by at most one block is
 * that footer; a rule anywhere else is content.
 */
function dropReleasePageFooter(blocks: NotesBlock[]): NotesBlock[] {
	let lastRule = -1
	for (let i = blocks.length - 1; i >= 0; i--) {
		if (blocks[i].type === 'rule') {
			lastRule = i
			break
		}
	}
	if (lastRule <= 0 || blocks.length - lastRule > 2) return blocks
	return blocks.slice(0, lastRule)
}

const REPO = 'tonyantony300/dashbeam'

export function releasePageUrl(version: string): string {
	return `https://github.com/${REPO}/releases/tag/v${version}`
}

export type ReleaseNotes = { body: string; url: string }

/** Throws on any failure; callers fall back to the release page link. */
export async function fetchReleaseNotes(
	version: string,
	signal?: AbortSignal
): Promise<ReleaseNotes> {
	const response = await fetch(
		`https://api.github.com/repos/${REPO}/releases/tags/v${encodeURIComponent(version)}`,
		{ headers: { Accept: 'application/vnd.github+json' }, signal }
	)
	if (!response.ok) {
		throw new Error(`Release notes request failed: ${response.status}`)
	}
	const release = (await response.json()) as {
		body?: string | null
		html_url?: string
	}
	const body = release.body?.trim()
	if (!body) throw new Error('Release has no notes')
	return { body, url: release.html_url ?? releasePageUrl(version) }
}

/** Numeric core of a version; pre-release and build suffixes are ignored. */
function versionCore(version: string): number[] {
	return version
		.trim()
		.replace(/^v/, '')
		.split(/[-+]/)[0]
		.split('.')
		.map((part) => Number.parseInt(part, 10) || 0)
}

export function isNewerVersion(candidate: string, baseline: string): boolean {
	const a = versionCore(candidate)
	const b = versionCore(baseline)
	for (let i = 0; i < Math.max(a.length, b.length); i++) {
		const diff = (a[i] ?? 0) - (b[i] ?? 0)
		if (diff !== 0) return diff > 0
	}
	return false
}

/**
 * Whether this launch is the first on a newer version. A missing record means
 * a fresh install (or the first build that keeps one): nothing to announce.
 */
export function shouldAnnounceUpdate(
	lastSeenVersion: string | null,
	currentVersion: string
): boolean {
	return (
		lastSeenVersion !== null && isNewerVersion(currentVersion, lastSeenVersion)
	)
}
