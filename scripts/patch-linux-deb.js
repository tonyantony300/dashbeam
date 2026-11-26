#!/usr/bin/env node

/**
 * Rewrites .deb dependency metadata so installs work on Debian and Ubuntu, then
 * re-signs the patched package and re-uploads it and its .sig to the GitHub
 * release (overwriting the assets that tauri-action just uploaded).
 *
 * Repacking changes the bytes, so the updater signature must be regenerated,
 * and the re-upload gets a new asset id. latest.json still holds the old id and
 * signature afterwards; finalize-updater-manifest.js repoints it once all build
 * jobs are done.
 *
 * Tauri's bundler emits Ubuntu 22.04 package names (libappindicator3-1,
 * libgtk-3-0). Debian and newer Ubuntu releases use libayatana-appindicator3-1
 * and libgtk-3-0t64 instead, so we declare both alternatives.
 *
 * Runs as a post-build step in CI. Re-upload is skipped when RELEASE_ID is not
 * set, and signing when TAURI_SIGNING_PRIVATE_KEY is not set, so the script can
 * also be run locally to patch a build.
 */

import fs from 'node:fs'
import path from 'node:path'
import { execSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const repoRoot = path.join(__dirname, '..')

const COMPATIBLE_DEPENDS =
	'libayatana-appindicator3-1 | libappindicator3-1, libwebkit2gtk-4.1-0, libgtk-3-0t64 | libgtk-3-0'

function findDebArtifacts() {
	const debDir = path.join(repoRoot, 'src-tauri/target/release/bundle/deb')
	if (!fs.existsSync(debDir)) {
		return []
	}

	return fs
		.readdirSync(debDir)
		.filter((name) => name.endsWith('.deb'))
		.map((name) => path.join(debDir, name))
}

function patchDeb(debPath) {
	const tmpDir = fs.mkdtempSync(path.join('/tmp', 'dashbeam-deb-'))
	try {
		execSync(
			`dpkg-deb -R ${JSON.stringify(debPath)} ${JSON.stringify(tmpDir)}`,
			{
				stdio: 'pipe',
			}
		)

		const controlPath = path.join(tmpDir, 'DEBIAN', 'control')
		const original = fs.readFileSync(controlPath, 'utf8')
		if (!/^Depends: .+$/m.test(original)) {
			throw new Error(`No Depends line found in ${debPath}`)
		}

		const patched = original.replace(
			/^Depends: .+$/m,
			`Depends: ${COMPATIBLE_DEPENDS}`
		)
		if (patched === original) {
			console.log(`Already compatible: ${debPath}`)
			return false
		}

		fs.writeFileSync(controlPath, patched)
		execSync(
			`dpkg-deb -b ${JSON.stringify(tmpDir)} ${JSON.stringify(debPath)}`,
			{
				stdio: 'pipe',
			}
		)

		console.log(`Patched ${debPath}`)
		console.log(`  Depends: ${COMPATIBLE_DEPENDS}`)
		return true
	} finally {
		fs.rmSync(tmpDir, { recursive: true, force: true })
	}
}

// Rewrites `${debPath}.sig` for the patched bytes.
function signDeb(debPath) {
	if (!process.env.TAURI_SIGNING_PRIVATE_KEY) {
		if (process.env.RELEASE_ID) {
			// An unsigned re-upload would break in-app updates for every .deb user.
			throw new Error(
				'TAURI_SIGNING_PRIVATE_KEY is required to re-sign the patched .deb'
			)
		}
		console.log('TAURI_SIGNING_PRIVATE_KEY not set; skipping re-sign.')
		return
	}

	// stdout is discarded: the CLI echoes the signature, which is already in the .sig.
	execSync(`pnpm tauri signer sign ${JSON.stringify(debPath)}`, {
		cwd: repoRoot,
		stdio: ['ignore', 'ignore', 'inherit'],
	})
	console.log(`Re-signed ${debPath}`)
}

function reuploadAsset(assetPath, contentType) {
	const releaseId = process.env.RELEASE_ID
	const repo = process.env.GITHUB_REPOSITORY
	if (!releaseId || !repo) {
		console.log('RELEASE_ID/GITHUB_REPOSITORY not set; skipping re-upload.')
		return
	}

	const name = path.basename(assetPath)
	// Paginated: a release has more assets than the default page of 30, and a
	// missed lookup skips the delete, so the upload below fails with a 422.
	const assetId = execSync(
		`gh api --paginate "repos/${repo}/releases/${releaseId}/assets?per_page=100" --jq ".[] | select(.name == \\"${name}\\") | .id // empty"`
	)
		.toString()
		.trim()

	if (assetId) {
		execSync(
			`gh api --method DELETE "repos/${repo}/releases/assets/${assetId}"`,
			{
				stdio: 'pipe',
			}
		)
	}

	execSync(
		`gh api --method POST -H "Content-Type: ${contentType}" --input ${JSON.stringify(assetPath)} "https://uploads.github.com/repos/${repo}/releases/${releaseId}/assets?name=${name}"`,
		{ stdio: 'pipe' }
	)
	console.log(`Re-uploaded ${name} to release ${releaseId}`)
}

const debPaths = findDebArtifacts()
if (debPaths.length === 0) {
	console.log('No .deb artifacts found; skipping dependency patch.')
} else {
	for (const debPath of debPaths) {
		const changed = patchDeb(debPath)
		if (changed) {
			signDeb(debPath)
			reuploadAsset(debPath, 'application/vnd.debian.binary-package')
			if (fs.existsSync(`${debPath}.sig`)) {
				reuploadAsset(`${debPath}.sig`, 'application/octet-stream')
			}
		}
	}
}
