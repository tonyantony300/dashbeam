import assert from 'node:assert/strict'
import { test } from 'node:test'
import { createUpdaterStore } from './updater-store.js'

test('starts idle with no version', () => {
	const store = createUpdaterStore()
	assert.equal(store.phase(), 'idle')
	assert.equal(store.version(), null)
	assert.equal(store.bannerVisible(), false)
})

test('updateFound moves to available and records the version', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	assert.equal(store.phase(), 'available')
	assert.equal(store.version(), '1.4.2')
})

test('banner becomes visible once an update is found', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	assert.equal(store.bannerVisible(), true)
})

test('dismiss hides the banner for that version', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.dismiss()
	assert.equal(store.bannerVisible(), false)
	assert.equal(store.dismissedVersion(), '1.4.2')
})

test('a re-check of a dismissed version keeps the banner hidden', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.dismiss()
	store.updateFound('1.4.2')
	assert.equal(store.bannerVisible(), false)
})

test('a newer version shows the banner again after a dismissal', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.dismiss()
	store.updateFound('1.5.0')
	assert.equal(store.bannerVisible(), true)
})

test('a version dismissed in a previous session stays hidden', () => {
	const store = createUpdaterStore({ dismissedVersion: '1.4.2' })
	store.updateFound('1.4.2')
	assert.equal(store.bannerVisible(), false)
})

test('startDownload moves to downloading', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	assert.equal(store.startDownload(), true)
	assert.equal(store.phase(), 'downloading')
})

test('startDownload is refused while a download is already running', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	assert.equal(store.startDownload(), false)
})

test('startDownload is refused when no update is available', () => {
	const store = createUpdaterStore()
	assert.equal(store.startDownload(), false)
	assert.equal(store.phase(), 'idle')
})

test('a dismissed update shows the banner again once downloading starts', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.dismiss()
	store.startDownload()
	assert.equal(store.bannerVisible(), true)
})

test('progress accumulates chunk lengths', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.addProgress(100)
	store.addProgress(50)
	assert.equal(store.downloadedBytes(), 150)
})

test('progressRatio is null until the content length is known', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.addProgress(100)
	assert.equal(store.progressRatio(), null)
})

test('progressRatio reflects downloaded over total', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.setContentLength(400)
	store.addProgress(100)
	assert.equal(store.progressRatio(), 0.25)
})

test('progressRatio never exceeds 1', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.setContentLength(100)
	store.addProgress(180)
	assert.equal(store.progressRatio(), 1)
})

// Downloading must not install: on Windows the installer quits the app, so
// it only starts once the user has agreed to restart.
test('downloadFinished moves to ready without installing', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.downloadFinished()
	assert.equal(store.phase(), 'ready')
})

test('startInstall moves a ready update to installing', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.downloadFinished()
	assert.equal(store.startInstall(), true)
	assert.equal(store.phase(), 'installing')
})

test('startInstall is refused before the download has finished', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	assert.equal(store.startInstall(), false)
	store.startDownload()
	assert.equal(store.startInstall(), false)
	assert.equal(store.phase(), 'downloading')
})

test('startInstall is refused while an install is already running', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.downloadFinished()
	store.startInstall()
	assert.equal(store.startInstall(), false)
})

test('installFailed returns to ready with the reason', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.downloadFinished()
	store.startInstall()
	store.installFailed('signature mismatch')
	assert.equal(store.phase(), 'ready')
	assert.equal(store.error(), 'signature mismatch')
	assert.equal(store.startInstall(), true)
	assert.equal(store.error(), null)
})

test('fail records the reason and a retry clears it', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.fail('network unreachable')
	assert.equal(store.error(), 'network unreachable')
	store.startDownload()
	assert.equal(store.error(), null)
})

test('a failure shows the banner even for a dismissed version', () => {
	const store = createUpdaterStore({ dismissedVersion: '1.4.2' })
	store.updateFound('1.4.2')
	store.startDownload()
	store.fail('network unreachable')
	assert.equal(store.bannerVisible(), true)
	store.dismiss()
	assert.equal(store.error(), null)
	assert.equal(store.bannerVisible(), false)
})

test('a re-check of the same version keeps the failure reason', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.fail('network unreachable')
	store.updateFound('1.4.2')
	assert.equal(store.error(), 'network unreachable')
	store.updateFound('1.5.0')
	assert.equal(store.error(), null)
})

test('updateFound keeps manifest notes and treats blank ones as absent', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2', null, '- fixes')
	assert.equal(store.notes(), '- fixes')
	store.updateFound('1.4.2', null, '  ')
	assert.equal(store.notes(), null)
})

test('markChecked records when the last check ran', () => {
	const store = createUpdaterStore({ lastCheckedAt: 100 })
	assert.equal(store.lastCheckedAt(), 100)
	store.markChecked(250)
	assert.equal(store.lastCheckedAt(), 250)
})

test('a ready update ignores a dismissal from before the download', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.dismiss()
	store.startDownload()
	store.downloadFinished()
	assert.equal(store.bannerVisible(), true)
})

test('fail returns to available so the user can retry', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.fail()
	assert.equal(store.phase(), 'available')
	assert.equal(store.startDownload(), true)
})

test('fail clears download progress', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.setContentLength(400)
	store.addProgress(100)
	store.fail()
	assert.equal(store.downloadedBytes(), 0)
	assert.equal(store.progressRatio(), null)
})

test('updateFound does not clobber an in-flight download', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.setContentLength(400)
	store.addProgress(100)
	store.updateFound('1.4.2')
	assert.equal(store.phase(), 'downloading')
	assert.equal(store.downloadedBytes(), 100)
})

test('noUpdate resets an available update to idle', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.noUpdate()
	assert.equal(store.phase(), 'idle')
	assert.equal(store.version(), null)
})

test('noUpdate is ignored while a download is running', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.noUpdate()
	assert.equal(store.phase(), 'downloading')
})

test('restarting moves to restarting', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	store.startDownload()
	store.downloadFinished()
	store.startInstall()
	store.restarting()
	assert.equal(store.phase(), 'restarting')
})

// Sideloaded Android cannot install in place, so the check carries the release
// page the user is handed off to instead.
test('updateFound records a download url when one is supplied', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2', 'https://example.test/releases/tag/v1.4.2')
	assert.equal(store.downloadUrl(), 'https://example.test/releases/tag/v1.4.2')
})

test('an update found without a url has none', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2')
	assert.equal(store.downloadUrl(), null)
})

test('noUpdate clears the download url', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2', 'https://example.test/releases/tag/v1.4.2')
	store.noUpdate()
	assert.equal(store.downloadUrl(), null)
})

test('a re-check that is ignored keeps the existing download url', () => {
	const store = createUpdaterStore()
	store.updateFound('1.4.2', 'https://example.test/releases/tag/v1.4.2')
	store.startDownload()
	store.updateFound('1.4.2')
	assert.equal(store.downloadUrl(), 'https://example.test/releases/tag/v1.4.2')
})
