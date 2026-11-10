#!/usr/bin/env node

/**
 * Makes the release's latest.json agree with the assets actually on the release,
 * then proves it: every updater entry must point at an existing asset whose
 * bytes verify against the updater public key.
 *
 * patch-linux-deb.js replaces the .deb (and its .sig) after tauri-action wrote
 * latest.json, so the -deb entries are repointed here. This runs once, after all
 * build jobs, because each matrix job rewrites latest.json in turn.
 *
 * Usage:
 *   RELEASE_ID=123 node scripts/finalize-updater-manifest.js      # repoint + verify + upload
 *   RELEASE_TAG=v0.7.2 node scripts/finalize-updater-manifest.js --check   # verify only
 *   ... --dry-run                                                 # repoint + verify, no upload
 *
 * Needs `gh` authenticated (GITHUB_TOKEN/GH_TOKEN) and GITHUB_REPOSITORY.
 */

import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { execFileSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const repoRoot = path.join(__dirname, '..')

const MANIFEST_NAME = 'latest.json'
// Updater platform key -> suffix of the .deb asset patch-linux-deb.js re-uploads.
const DEB_PLATFORMS = {
	'linux-x86_64-deb': '_amd64.deb',
	'linux-aarch64-deb': '_arm64.deb',
}
const ED25519_SPKI_PREFIX = Buffer.from('302a300506032b6570032100', 'hex')

const checkOnly = process.argv.includes('--check')
const dryRun = process.argv.includes('--dry-run')
const repo = process.env.GITHUB_REPOSITORY
if (!repo) {
	fail('GITHUB_REPOSITORY is not set (e.g. owner/name).')
}

function fail(message) {
	console.error(`ERROR: ${message}`)
	process.exit(1)
}

function gh(args, options = {}) {
	return execFileSync('gh', args, {
		maxBuffer: 1024 * 1024 * 1024,
		stdio: ['ignore', 'pipe', 'inherit'],
		...options,
	})
}

function resolveReleaseId() {
	if (process.env.RELEASE_ID) return process.env.RELEASE_ID
	const tag = process.env.RELEASE_TAG
	if (!tag) fail('Set RELEASE_ID or RELEASE_TAG.')
	return gh(['api', `repos/${repo}/releases/tags/${tag}`, '--jq', '.id'])
		.toString()
		.trim()
}

function listAssets(releaseId) {
	return gh([
		'api',
		'--paginate',
		`repos/${repo}/releases/${releaseId}/assets?per_page=100`,
		'--jq',
		'.[] | {id, name}',
	])
		.toString()
		.split('\n')
		.filter(Boolean)
		.map((line) => JSON.parse(line))
}

function downloadAsset(id) {
	return gh([
		'api',
		'-H',
		'Accept: application/octet-stream',
		`repos/${repo}/releases/assets/${id}`,
	])
}

function assetUrl(id) {
	return `https://api.github.com/repos/${repo}/releases/assets/${id}`
}

// Tauri wraps minisign's key and signature files in one more layer of base64.
function loadPublicKey() {
	const conf = JSON.parse(
		fs.readFileSync(path.join(repoRoot, 'src-tauri/tauri.conf.json'), 'utf8')
	)
	const text = Buffer.from(conf.plugins.updater.pubkey, 'base64').toString()
	const raw = Buffer.from(text.trim().split('\n').pop(), 'base64')
	return {
		keyId: raw.subarray(2, 10),
		key: crypto.createPublicKey({
			key: Buffer.concat([ED25519_SPKI_PREFIX, raw.subarray(10, 42)]),
			format: 'der',
			type: 'spki',
		}),
	}
}

/** Returns null when `signature` is valid for `bytes`, else the reason it is not. */
function verifySignature(signature, bytes, publicKey) {
	const lines = Buffer.from(signature, 'base64').toString().split('\n')
	const raw = Buffer.from(lines[1] ?? '', 'base64')
	if (raw.length !== 74) return 'malformed signature'

	const algorithm = raw.subarray(0, 2).toString()
	if (!raw.subarray(2, 10).equals(publicKey.keyId)) {
		return 'signed with a different key'
	}
	// "ED" signs the BLAKE2b-512 digest (what Tauri emits); "Ed" signs the file.
	const message =
		algorithm === 'ED'
			? crypto.createHash('blake2b512').update(bytes).digest()
			: bytes
	const sig = raw.subarray(10)
	if (!crypto.verify(null, message, publicKey.key, sig)) {
		return 'signature does not match the asset bytes'
	}

	const trusted = (lines[2] ?? '').replace(/^trusted comment: /, '')
	const globalSig = Buffer.from(lines[3] ?? '', 'base64')
	const globalMessage = Buffer.concat([sig, Buffer.from(trusted)])
	if (!crypto.verify(null, globalMessage, publicKey.key, globalSig)) {
		return 'trusted comment signature is invalid'
	}
	return null
}

function repointDebEntries(manifest, assets) {
	let changed = false
	for (const [platform, suffix] of Object.entries(DEB_PLATFORMS)) {
		const entry = manifest.platforms[platform]
		if (!entry) continue

		const debs = assets.filter((asset) => asset.name.endsWith(suffix))
		if (debs.length !== 1) {
			fail(
				`expected one *${suffix} asset for ${platform}, found ${debs.length}`
			)
		}
		const sig = assets.find((asset) => asset.name === `${debs[0].name}.sig`)
		if (!sig) fail(`${debs[0].name}.sig is missing from the release`)

		const url = assetUrl(debs[0].id)
		const signature = downloadAsset(sig.id).toString().trim()
		if (entry.url !== url || entry.signature !== signature) {
			entry.url = url
			entry.signature = signature
			changed = true
			console.log(`Repointed ${platform} -> ${debs[0].name} (${debs[0].id})`)
		}
	}
	return changed
}

function verifyManifest(manifest, assets, publicKey) {
	const byId = new Map(assets.map((asset) => [String(asset.id), asset]))
	const results = new Map() // asset id + signature -> failure reason or null
	const failures = []

	for (const [platform, entry] of Object.entries(manifest.platforms)) {
		const id = /\/releases\/assets\/(\d+)$/.exec(entry.url ?? '')?.[1]
		const asset = id && byId.get(id)
		let reason
		if (!id) {
			reason = `unexpected url ${entry.url}`
		} else if (!asset) {
			reason = `asset ${id} is not on the release (download would 404)`
		} else {
			const cacheKey = `${id}:${entry.signature}`
			if (!results.has(cacheKey)) {
				results.set(
					cacheKey,
					verifySignature(entry.signature, downloadAsset(id), publicKey)
				)
			}
			reason = results.get(cacheKey)
		}

		if (reason) {
			failures.push(`${platform}: ${reason}`)
			console.log(`FAIL  ${platform}: ${reason}`)
		} else {
			console.log(`ok    ${platform} -> ${asset.name}`)
		}
	}
	return failures
}

function uploadManifest(releaseId, manifest, previousAssetId) {
	const file = path.join(
		fs.mkdtempSync(path.join(os.tmpdir(), 'dashbeam-manifest-')),
		MANIFEST_NAME
	)
	fs.writeFileSync(file, `${JSON.stringify(manifest, null, 2)}\n`)
	// GitHub has no in-place asset update: delete, then upload under the same name.
	gh([
		'api',
		'--method',
		'DELETE',
		`repos/${repo}/releases/assets/${previousAssetId}`,
	])
	gh([
		'api',
		'--method',
		'POST',
		'-H',
		'Content-Type: application/json',
		'--input',
		file,
		`https://uploads.github.com/repos/${repo}/releases/${releaseId}/assets?name=${MANIFEST_NAME}`,
	])
	console.log(`Uploaded corrected ${MANIFEST_NAME}`)
}

const releaseId = resolveReleaseId()
const assets = listAssets(releaseId)
const manifestAsset = assets.find((asset) => asset.name === MANIFEST_NAME)
if (!manifestAsset)
	fail(`${MANIFEST_NAME} is missing from release ${releaseId}`)

const manifest = JSON.parse(downloadAsset(manifestAsset.id).toString())
const changed = checkOnly ? false : repointDebEntries(manifest, assets)

const failures = verifyManifest(manifest, assets, loadPublicKey())
if (failures.length > 0) {
	fail(
		`${failures.length} updater entr${failures.length === 1 ? 'y' : 'ies'} would fail in-app update; ${MANIFEST_NAME} left untouched.`
	)
}

if (changed && dryRun) {
	console.log(`Dry run: not uploading the corrected ${MANIFEST_NAME}.`)
} else if (changed) {
	uploadManifest(releaseId, manifest, manifestAsset.id)
} else {
	console.log(`${MANIFEST_NAME} already matches the release assets.`)
}
