#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const rootDir = path.resolve(__dirname, '..')
const genAndroid = path.join(rootDir, 'src-tauri/gen/android')
const universalApkDir = path.join(
	genAndroid,
	'app/build/outputs/apk/universal/release'
)
const extraSignedDir = path.join(rootDir, 'build/android-apks')

const bundleDir = path.join(genAndroid, 'app/build/outputs/bundle')
const signedAabDir = path.join(rootDir, 'build/android-aab')

const REQUIRED_UNIVERSAL_ABIS = ['arm64-v8a', 'armeabi-v7a']
const INSTALL_PACKAGES_PERMISSION =
	'android.permission.REQUEST_INSTALL_PACKAGES'

/** @type {Record<string, { buildArgs: string[], signedFileName: string, signedDir: string }>} */
const APK_PROFILES = {
	universal: {
		buildArgs: ['--apk'],
		signedFileName: 'app-universal-release.apk',
		signedDir: extraSignedDir,
	},
}

for (const [name, target] of Object.entries({
	arm64: 'aarch64',
	armv7: 'armv7',
})) {
	APK_PROFILES[name] = {
		buildArgs: ['--apk', '--split-per-abi', '--target', target],
		signedFileName: `app-${name}-release.apk`,
		signedDir: extraSignedDir,
	}
}

/**
 * Play wants one universal bundle and splits it per device itself, so `--aab`
 * runs without `--split-per-abi`.
 */
const AAB_PROFILES = {
	universal: {
		buildArgs: ['--aab'],
		outputDir: path.join(bundleDir, 'universalRelease'),
		signedFileName: 'app-universal-release.aab',
	},
}

/** Tauri `--split-per-abi` Gradle output folder names (not jni lib ABI names). */
const PROFILE_ABI_DIRS = {
	arm64: 'arm64',
	armv7: 'arm',
}

function outputDirForProfile(profileName) {
	if (profileName === 'universal') {
		return universalApkDir
	}
	const abi = PROFILE_ABI_DIRS[profileName]
	if (!abi) {
		throw new Error(
			`android-release-build: no APK output dir for profile "${profileName}"`
		)
	}
	return path.join(genAndroid, 'app/build/outputs/apk', abi, 'release')
}

/** @returns {{ apk: string, gradleSigned: boolean } | null} */
function findApkInDir(dir) {
	if (!fs.existsSync(dir)) {
		return null
	}
	const files = fs.readdirSync(dir).filter((f) => f.endsWith('.apk'))
	const unsigned = files.find((f) => f.endsWith('-unsigned.apk'))
	if (unsigned) {
		return { apk: path.join(dir, unsigned), gradleSigned: false }
	}
	const signed = files.find((f) => !f.endsWith('-unsigned.apk'))
	if (signed) {
		return { apk: path.join(dir, signed), gradleSigned: true }
	}
	return null
}

/** @returns {{ apk: string, gradleSigned: boolean } | null} */
function resolveApkAfterBuild(profileName) {
	return findApkInDir(outputDirForProfile(profileName))
}

function verifyUniversalApk(apkPath) {
	const listing = spawnSync('unzip', ['-l', apkPath], { encoding: 'utf8' })
	if (listing.status !== 0) {
		console.error(
			'android-release-build: failed to inspect universal APK:',
			apkPath
		)
		process.exit(1)
	}
	const missing = REQUIRED_UNIVERSAL_ABIS.filter(
		(abi) => !listing.stdout.includes(`lib/${abi}/`)
	)
	if (missing.length > 0) {
		console.error(
			`android-release-build: universal APK is missing native libs for: ${missing.join(', ')}`,
			`\n  ${apkPath}`,
			'\n  Per-ABI builds must not overwrite the universal output; check build order and --split-per-abi.'
		)
		process.exit(1)
	}
	console.log(
		`android-release-build: verified universal APK contains all ABIs (${REQUIRED_UNIVERSAL_ABIS.join(', ')})`
	)
}

function findAabInDir(dir) {
	if (!fs.existsSync(dir)) {
		return null
	}
	const aab = fs.readdirSync(dir).find((f) => f.endsWith('.aab'))
	return aab ? path.join(dir, aab) : null
}

function verifyBundleAbis(aabPath) {
	const listing = spawnSync('unzip', ['-l', aabPath], { encoding: 'utf8' })
	if (listing.status !== 0) {
		console.error('android-release-build: failed to inspect AAB:', aabPath)
		process.exit(1)
	}
	const missing = REQUIRED_UNIVERSAL_ABIS.filter(
		(abi) => !listing.stdout.includes(`base/lib/${abi}/`)
	)
	if (missing.length > 0) {
		console.error(
			`android-release-build: AAB is missing native libs for: ${missing.join(', ')}`,
			`\n  ${aabPath}`
		)
		process.exit(1)
	}
	console.log(
		`android-release-build: verified AAB contains all ABIs (${REQUIRED_UNIVERSAL_ABIS.join(', ')})`
	)
}

/** Play forbids REQUEST_INSTALL_PACKAGES, so the AAB must not carry it. */
function verifyAabOmitsInstallPermission(aabPath) {
	const r = spawnSync(
		'unzip',
		['-p', aabPath, 'base/manifest/AndroidManifest.xml'],
		{ encoding: 'buffer' }
	)
	if (r.status !== 0) {
		console.error(
			'android-release-build: failed to read the AAB manifest:',
			aabPath
		)
		process.exit(1)
	}
	if (r.stdout.includes(Buffer.from(INSTALL_PACKAGES_PERMISSION))) {
		console.error(
			'android-release-build: Play AAB still declares REQUEST_INSTALL_PACKAGES:',
			`\n  ${aabPath}`
		)
		process.exit(1)
	}
	console.log(
		'android-release-build: verified Play AAB omits REQUEST_INSTALL_PACKAGES'
	)
}

/**
 * Gradle signs the bundle in place and keeps the same file name either way
 * (unlike APKs, which get an `-unsigned` suffix), so the signature itself is
 * the only reliable signal. Play rejects an unsigned upload.
 */
function verifyBundleSigned(aabPath) {
	const r = spawnSync('jarsigner', ['-verify', aabPath], { encoding: 'utf8' })
	if (r.error) {
		console.error(
			'android-release-build: jarsigner not found; cannot verify the AAB signature.',
			'\n  Install a JDK, or drop keystore.properties to build an unsigned AAB.'
		)
		process.exit(1)
	}
	if (r.status !== 0 || !r.stdout.includes('jar verified')) {
		console.error(
			'android-release-build: AAB is not signed (Gradle release signingConfig did not apply):',
			`\n  ${aabPath}`
		)
		console.error(r.stdout || r.stderr)
		process.exit(1)
	}
	console.log('android-release-build: verified AAB signature')
}

function run(cmd, args, opts = {}) {
	const cwd = opts.cwd ?? rootDir
	const env = { ...process.env, ...opts.env }
	if (opts.noCi) {
		delete env.CI
	}
	const r = spawnSync(cmd, args, { stdio: 'inherit', cwd, env })
	if (r.status !== 0) {
		process.exit(r.status ?? 1)
	}
}

function resolveApksigner() {
	const androidHome =
		process.env.ANDROID_HOME ||
		process.env.ANDROID_SDK_ROOT ||
		path.join(process.env.HOME || '', 'Library/Android/sdk')
	let apksigner = path.join(androidHome, 'build-tools', '34.0.0', 'apksigner')
	if (!fs.existsSync(apksigner)) {
		const buildTools = path.join(androidHome, 'build-tools')
		if (fs.existsSync(buildTools)) {
			const versions = fs.readdirSync(buildTools).sort().reverse()
			for (const v of versions) {
				const p = path.join(buildTools, v, 'apksigner')
				if (fs.existsSync(p)) {
					apksigner = p
					break
				}
			}
		}
	}
	if (!fs.existsSync(apksigner)) {
		console.error(
			'android-release-build: apksigner not found. Set ANDROID_HOME and ensure build-tools is installed.'
		)
		process.exit(1)
	}
	return apksigner
}

function readKeystoreProps() {
	const keystorePropsPath = path.join(genAndroid, 'keystore.properties')
	if (!fs.existsSync(keystorePropsPath)) {
		return null
	}
	const props = Object.fromEntries(
		fs
			.readFileSync(keystorePropsPath, 'utf8')
			.split('\n')
			.filter((l) => l && !l.startsWith('#'))
			.map((l) => {
				const i = l.indexOf('=')
				return [l.slice(0, i).trim(), l.slice(i + 1).trim()]
			})
	)
	const storeFile = props.storeFile || props.store
	const alias = props.keyAlias || props.alias
	const ksPassword = props.storePassword || props.password
	const keyPass = props.keyPassword || props.password
	if (!storeFile || !alias || !ksPassword || !keyPass) {
		return null
	}
	return { storeFile, alias, ksPassword, keyPass }
}

function signApk(unsignedApk, signedApk, keystore) {
	fs.mkdirSync(path.dirname(signedApk), { recursive: true })
	const apksigner = resolveApksigner()
	const ksPassEnvVar = 'ALTSENDME_APKSIGNER_KS_PASS'
	const keyPassEnvVar = 'ALTSENDME_APKSIGNER_KEY_PASS'
	const r = spawnSync(
		apksigner,
		[
			'sign',
			'--ks',
			keystore.storeFile,
			'--ks-key-alias',
			keystore.alias,
			'--ks-pass',
			`env:${ksPassEnvVar}`,
			'--key-pass',
			`env:${keyPassEnvVar}`,
			'--out',
			signedApk,
			unsignedApk,
		],
		{
			stdio: 'inherit',
			cwd: rootDir,
			env: {
				...process.env,
				[ksPassEnvVar]: keystore.ksPassword,
				[keyPassEnvVar]: keystore.keyPass,
			},
		}
	)
	if (r.status !== 0) {
		process.exit(r.status ?? 1)
	}
	console.log('\nSigned APK:', signedApk)
}

/**
 * `??` rather than `||` so an explicitly empty list means "build none" — that
 * is how the AAB-only CI job skips APKs.
 */
function selectProfiles(table, envVar, fallback) {
	const raw = process.env[envVar] ?? fallback
	const names = raw
		.split(',')
		.map((s) => s.trim())
		.filter(Boolean)
	const unknown = names.filter((n) => !table[n])
	if (unknown.length > 0) {
		console.error(
			`android-release-build: unknown ${envVar} profile(s): ${unknown.join(', ')}`,
			`(valid: ${Object.keys(table).join(', ')})`
		)
		process.exit(1)
	}
	return names.map((name) => ({ name, ...table[name] }))
}

if (fs.existsSync(genAndroid)) {
	console.log(
		'android-release-build: removing gen/android before tauri android init'
	)
	fs.rmSync(genAndroid, { recursive: true, force: true })
}
console.log(
	'android-release-build: tauri android init (generating Gradle build files)'
)
run('npx', ['tauri', 'android', 'init', '--ci'], { noCi: true })

console.log(
	'android-release-build: restoring committed gen/android assets from git'
)
run('git', ['checkout', 'HEAD', '--', 'src-tauri/gen/android/app/src/main/'])

const manifestPath = path.join(genAndroid, 'app/src/main/AndroidManifest.xml')
if (!fs.existsSync(manifestPath)) {
	console.error(
		'android-release-build: AndroidManifest.xml missing after init + git restore:',
		manifestPath
	)
	process.exit(1)
}

const buildGradle = path.join(genAndroid, 'app/build.gradle.kts')
if (!fs.existsSync(buildGradle)) {
	console.error(
		'android-release-build: build.gradle.kts missing after init:',
		buildGradle
	)
	process.exit(1)
}

let manifestBeforePlayEdit = null

/** Strip the plugin-declared permission from the generated manifest for AAB builds. */
function removeInstallPermissionForPlay() {
	manifestBeforePlayEdit = fs.readFileSync(manifestPath, 'utf8')
	let manifest = manifestBeforePlayEdit
	if (!manifest.includes('xmlns:tools=')) {
		manifest = manifest.replace(
			'<manifest ',
			'<manifest xmlns:tools="http://schemas.android.com/tools" '
		)
	}
	manifest = manifest.replace(
		'</manifest>',
		`    <uses-permission\n        android:name="${INSTALL_PACKAGES_PERMISSION}"\n        tools:node="remove" />\n</manifest>`
	)
	fs.writeFileSync(manifestPath, manifest)
	console.log(
		'android-release-build: removing REQUEST_INSTALL_PACKAGES from the Play manifest'
	)
}

/** Restore the shared generated manifest after the Play-only edit. */
function restoreInstallPermissionManifest() {
	if (manifestBeforePlayEdit === null) return
	try {
		fs.writeFileSync(manifestPath, manifestBeforePlayEdit)
		manifestBeforePlayEdit = null
		console.log('android-release-build: restored the generated manifest')
	} catch (e) {
		console.error(
			'android-release-build: failed to restore the generated manifest:',
			e
		)
	}
}

process.on('exit', restoreInstallPermissionManifest)

/**
 * `MainActivity.kt` is generated by `tauri android init` and is not tracked, so a
 * Tauri upgrade could silently drop the edge-to-edge call. Without it the WebView
 * stops rendering behind the system bars below API 35 and Play flags the bundle.
 */
const mainActivity = path.join(
	genAndroid,
	'app/src/main/java/com/dashbeam/android/MainActivity.kt'
)
const mainActivitySource = fs.existsSync(mainActivity)
	? fs.readFileSync(mainActivity, 'utf8')
	: ''
if (!mainActivitySource.includes('enableEdgeToEdge()')) {
	console.error(
		'android-release-build: MainActivity.kt does not call enableEdgeToEdge():',
		`\n  ${mainActivity}`,
		'\n  The Tauri template changed. Restore the call before releasing.'
	)
	process.exit(1)
}

/**
 * Play's developer-verification flow requires a token file inside the APK's
 * assets. Written after the git restore so it survives `tauri android init`,
 * and only when a token is supplied, so it stays out of shipped builds.
 */
const adiToken = process.env.ANDROID_ADI_TOKEN
if (adiToken) {
	const assetsDir = path.join(genAndroid, 'app/src/main/assets')
	fs.mkdirSync(assetsDir, { recursive: true })
	const adiFile = path.join(assetsDir, 'adi-registration.properties')
	fs.writeFileSync(adiFile, `${adiToken.trim()}\n`)
	console.log('android-release-build: wrote', adiFile)
}

const keyBase64 = process.env.ANDROID_KEY_BASE64
const keyAlias = process.env.ANDROID_KEY_ALIAS
const keyPassword = process.env.ANDROID_KEY_PASSWORD
const storePassword = process.env.ANDROID_STORE_PASSWORD || keyPassword
if (keyBase64 && keyAlias && keyPassword) {
	const keystorePath = path.join(rootDir, '.keystore.jks')
	fs.writeFileSync(keystorePath, Buffer.from(keyBase64, 'base64'), {
		mode: 0o600,
	})
	fs.writeFileSync(
		path.join(genAndroid, 'keystore.properties'),
		`keyAlias=${keyAlias}\nkeyPassword=${keyPassword}\nstoreFile=${path.resolve(keystorePath)}\nstorePassword=${storePassword}\n`,
		{ mode: 0o600 }
	)
}

run('node', [path.join(__dirname, 'apply-android-release-gradle-patches.js')])

const keystore = readKeystoreProps()
const profiles = selectProfiles(
	APK_PROFILES,
	'ANDROID_APK_PROFILES',
	'universal,arm64,armv7'
)
const aabProfiles = selectProfiles(AAB_PROFILES, 'ANDROID_AAB_PROFILES', '')

for (const profile of profiles) {
	console.log(`\nandroid-release-build: building profile "${profile.name}"`)
	run(
		'npx',
		['tauri', 'android', 'build', ...profile.buildArgs, '--', '--locked'],
		{
			noCi: true,
		}
	)

	const built = resolveApkAfterBuild(profile.name)
	if (!built) {
		console.error(
			`android-release-build: APK not found for profile "${profile.name}"`,
			`(checked ${outputDirForProfile(profile.name)})`
		)
		process.exit(1)
	}

	const signedApk = path.join(profile.signedDir, profile.signedFileName)
	if (built.gradleSigned) {
		fs.mkdirSync(path.dirname(signedApk), { recursive: true })
		if (path.resolve(built.apk) !== path.resolve(signedApk)) {
			fs.copyFileSync(built.apk, signedApk)
		}
		console.log('\nSigned APK (Gradle):', signedApk)
	} else if (keystore) {
		signApk(built.apk, signedApk, keystore)
	} else {
		const dest = signedApk.replace(/\.apk$/, '-unsigned.apk')
		fs.mkdirSync(path.dirname(dest), { recursive: true })
		fs.copyFileSync(built.apk, dest)
		console.log(`\nUnsigned APK (no keystore): ${dest}`)
	}

	if (profile.name === 'universal') {
		verifyUniversalApk(signedApk)
	}
}

if (aabProfiles.length > 0) {
	removeInstallPermissionForPlay()
}

for (const profile of aabProfiles) {
	console.log(`\nandroid-release-build: building AAB profile "${profile.name}"`)
	run(
		'npx',
		['tauri', 'android', 'build', ...profile.buildArgs, '--', '--locked'],
		{
			noCi: true,
		}
	)

	const built = findAabInDir(profile.outputDir)
	if (!built) {
		console.error(
			`android-release-build: AAB not found for profile "${profile.name}"`,
			`(checked ${profile.outputDir})`
		)
		process.exit(1)
	}

	const suffix = keystore ? '' : '-unsigned'
	const dest = path.join(
		signedAabDir,
		profile.signedFileName.replace(/\.aab$/, `${suffix}.aab`)
	)
	fs.mkdirSync(path.dirname(dest), { recursive: true })
	fs.copyFileSync(built, dest)

	verifyBundleAbis(dest)
	verifyAabOmitsInstallPermission(dest)
	if (keystore) {
		verifyBundleSigned(dest)
		console.log('\nSigned AAB (Gradle):', dest)
	} else {
		console.log(`\nUnsigned AAB (no keystore): ${dest}`)
	}
}

restoreInstallPermissionManifest()
