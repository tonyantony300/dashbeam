import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
	isNewerVersion,
	parseInline,
	parseReleaseNotes,
	releasePageUrl,
	shouldAnnounceUpdate,
} from './release-notes.js'

test('plain text is a single text node', () => {
	assert.deepEqual(parseInline('Just words'), [
		{ type: 'text', text: 'Just words' },
	])
})

test('bold, code and links are recognised inline', () => {
	assert.deepEqual(
		parseInline('**Tray:** update `iroh` see [docs](https://example.test/d)'),
		[
			{ type: 'bold', children: [{ type: 'text', text: 'Tray:' }] },
			{ type: 'text', text: ' update ' },
			{ type: 'code', text: 'iroh' },
			{ type: 'text', text: ' see ' },
			{ type: 'link', text: 'docs', href: 'https://example.test/d' },
		]
	)
})

test('a bare url becomes a link without its trailing punctuation', () => {
	assert.deepEqual(parseInline('Details: https://example.test/blog.'), [
		{ type: 'text', text: 'Details: ' },
		{
			type: 'link',
			text: 'https://example.test/blog',
			href: 'https://example.test/blog',
		},
		{ type: 'text', text: '.' },
	])
})

test('a non-http link target stays plain text', () => {
	assert.deepEqual(parseInline('[click](javascript:alert(1))'), [
		{ type: 'text', text: '[click](javascript:alert(1))' },
	])
})

test('markup inside bold is parsed', () => {
	assert.deepEqual(parseInline('**use `x`**'), [
		{
			type: 'bold',
			children: [
				{ type: 'text', text: 'use ' },
				{ type: 'code', text: 'x' },
			],
		},
	])
})

test('headings, paragraphs and lists become blocks', () => {
	const blocks = parseReleaseNotes(
		[
			'### Highlights',
			'',
			'First line',
			'second line',
			'',
			'- one',
			'- two',
		].join('\n')
	)
	assert.deepEqual(blocks, [
		{
			type: 'heading',
			level: 3,
			inline: [{ type: 'text', text: 'Highlights' }],
		},
		{
			type: 'paragraph',
			inline: [{ type: 'text', text: 'First line second line' }],
		},
		{
			type: 'list',
			ordered: false,
			items: [[{ type: 'text', text: 'one' }], [{ type: 'text', text: 'two' }]],
		},
	])
})

test('numbered items form an ordered list', () => {
	const [list] = parseReleaseNotes('1. first\n2. second')
	assert.equal(list.type, 'list')
	assert.equal(list.type === 'list' && list.ordered, true)
	assert.equal(list.type === 'list' && list.items.length, 2)
})

test('an indented line continues the list item above it', () => {
	const [list] = parseReleaseNotes('- one\n  more\n- two')
	assert.deepEqual(list, {
		type: 'list',
		ordered: false,
		items: [
			[{ type: 'text', text: 'one more' }],
			[{ type: 'text', text: 'two' }],
		],
	})
})

test('windows line endings parse the same', () => {
	assert.deepEqual(
		parseReleaseNotes('## A\r\n\r\n- x\r\n'),
		parseReleaseNotes('## A\n\n- x\n')
	)
})

test('the release-page footer after the final rule is dropped', () => {
	const blocks = parseReleaseNotes(
		'- fix\n\n---\n\nSee the assets below to download and install this version.'
	)
	assert.equal(blocks.length, 1)
	assert.equal(blocks[0].type, 'list')
})

test('a rule in the middle of the notes is kept', () => {
	const blocks = parseReleaseNotes('one\n\n---\n\ntwo\n\nthree')
	assert.deepEqual(
		blocks.map((block) => block.type),
		['paragraph', 'rule', 'paragraph', 'paragraph']
	)
})

test('empty notes yield no blocks', () => {
	assert.deepEqual(parseReleaseNotes('  \n\n'), [])
})

test('releasePageUrl points at the version tag', () => {
	assert.equal(
		releasePageUrl('0.7.3'),
		'https://github.com/tonyantony300/dashbeam/releases/tag/v0.7.3'
	)
})

test('isNewerVersion compares components numerically', () => {
	assert.equal(isNewerVersion('0.10.0', '0.9.0'), true)
	assert.equal(isNewerVersion('0.7.3', '0.7.3'), false)
	assert.equal(isNewerVersion('0.7.2', '0.7.3'), false)
	assert.equal(isNewerVersion('v0.8', '0.7.9'), true)
})

test('an update is announced only on the first launch of a newer version', () => {
	assert.equal(shouldAnnounceUpdate('0.7.2', '0.7.3'), true)
	assert.equal(shouldAnnounceUpdate('0.7.3', '0.7.3'), false)
	// Fresh install: nothing was updated.
	assert.equal(shouldAnnounceUpdate(null, '0.7.3'), false)
	// A downgrade is not news.
	assert.equal(shouldAnnounceUpdate('0.8.0', '0.7.3'), false)
})
