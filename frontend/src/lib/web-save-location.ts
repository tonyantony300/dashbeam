import { triggerBrowserDownload } from './wasm-bridge-client'
import { createStoreOnlyZip } from './web-zip'

const DB_NAME = 'alt-sendme'
const STORE_NAME = 'handles'
const DIR_HANDLE_KEY = 'download-directory'

let cachedDirHandle: FileSystemDirectoryHandle | null = null

export type ReceivedWebFile = {
	name: string
	bytes: Uint8Array
}

export type WebWriteConflict = {
	original: string
	resolved: string
}

export type WriteCollectionResult = {
	savedToDirectory: boolean
	zippedDownload: boolean
	conflicts: WebWriteConflict[]
}

function openDb(): Promise<IDBDatabase> {
	return new Promise((resolve, reject) => {
		const request = indexedDB.open(DB_NAME, 1)
		request.onerror = () => reject(request.error)
		request.onsuccess = () => resolve(request.result)
		request.onupgradeneeded = () => {
			request.result.createObjectStore(STORE_NAME)
		}
	})
}

async function idbGet<T>(key: string): Promise<T | undefined> {
	const db = await openDb()
	return new Promise((resolve, reject) => {
		const tx = db.transaction(STORE_NAME, 'readonly')
		const req = tx.objectStore(STORE_NAME).get(key)
		req.onsuccess = () => resolve(req.result as T | undefined)
		req.onerror = () => reject(req.error)
	})
}

async function idbSet(key: string, value: unknown): Promise<void> {
	const db = await openDb()
	return new Promise((resolve, reject) => {
		const tx = db.transaction(STORE_NAME, 'readwrite')
		const req = tx.objectStore(STORE_NAME).put(value, key)
		req.onsuccess = () => resolve()
		req.onerror = () => reject(req.error)
	})
}

export function supportsWebSaveLocationPicker(): boolean {
	return (
		typeof window !== 'undefined' &&
		window.isSecureContext &&
		'showDirectoryPicker' in window
	)
}

async function ensureDirWritePermission(
	handle: FileSystemDirectoryHandle
): Promise<boolean> {
	const current = await handle.queryPermission({ mode: 'readwrite' })
	if (current === 'granted') {
		return true
	}
	const requested = await handle.requestPermission({ mode: 'readwrite' })
	return requested === 'granted'
}

async function saveDirectoryHandle(
	handle: FileSystemDirectoryHandle
): Promise<void> {
	await idbSet(DIR_HANDLE_KEY, handle)
	cachedDirHandle = handle
}

async function loadSavedDirectoryHandle(): Promise<FileSystemDirectoryHandle | null> {
	try {
		const handle = await idbGet<FileSystemDirectoryHandle>(DIR_HANDLE_KEY)
		if (!handle) {
			return null
		}

		cachedDirHandle = handle
		return handle
	} catch (error) {
		console.warn('Failed to load saved download directory:', error)
		return null
	}
}

export async function initWebSaveLocation(): Promise<string> {
	const handle = await loadSavedDirectoryHandle()
	return handle?.name ?? ''
}

export async function pickDownloadDirectory(): Promise<string | null> {
	if (!supportsWebSaveLocationPicker()) {
		return null
	}

	try {
		const handle = await window.showDirectoryPicker!({ mode: 'readwrite' })
		await saveDirectoryHandle(handle)
		return handle.name
	} catch (error) {
		if (error instanceof DOMException && error.name === 'AbortError') {
			return null
		}
		// A repeat tap while the picker is open (common on mobile Chromium);
		// the open picker still applies its own result.
		if (
			error instanceof DOMException &&
			error.name === 'NotAllowedError' &&
			/already active/i.test(error.message)
		) {
			return null
		}
		throw error
	}
}

function basename(path: string): string {
	const normalized = path.replace(/\\/g, '/')
	return normalized.split('/').pop() ?? path
}

function resolveConflictName(fileName: string, index: number): string {
	const dot = fileName.lastIndexOf('.')
	if (dot > 0) {
		return `${fileName.slice(0, dot)} (${index})${fileName.slice(dot)}`
	}
	return `${fileName} (${index})`
}

async function fileExists(
	dirHandle: FileSystemDirectoryHandle,
	fileName: string
): Promise<boolean> {
	try {
		await dirHandle.getFileHandle(fileName, { create: false })
		return true
	} catch {
		return false
	}
}

async function resolveWritableFileName(
	dirHandle: FileSystemDirectoryHandle,
	fileName: string
): Promise<{ fileName: string; conflict?: WebWriteConflict }> {
	let candidate = fileName
	for (let index = 1; index < 10_000; index++) {
		if (!(await fileExists(dirHandle, candidate))) {
			if (candidate === fileName) {
				return { fileName: candidate }
			}
			return {
				fileName: candidate,
				conflict: { original: fileName, resolved: candidate },
			}
		}
		candidate = resolveConflictName(fileName, index)
	}

	throw new Error(`Too many filename conflicts for ${fileName}`)
}

async function getDirectoryForRelativePath(
	root: FileSystemDirectoryHandle,
	relativePath: string
): Promise<{ dirHandle: FileSystemDirectoryHandle; fileName: string }> {
	const parts = relativePath.replace(/\\/g, '/').split('/').filter(Boolean)
	if (!parts.length) {
		throw new Error('Invalid relative path')
	}

	const fileName = parts.pop()!
	let current = root
	for (const part of parts) {
		current = await current.getDirectoryHandle(part, { create: true })
	}

	return { dirHandle: current, fileName }
}

async function writeBytesToDirectory(
	dirHandle: FileSystemDirectoryHandle,
	fileName: string,
	bytes: Uint8Array
): Promise<WebWriteConflict | undefined> {
	const resolved = await resolveWritableFileName(dirHandle, fileName)
	const fileHandle = await dirHandle.getFileHandle(resolved.fileName, {
		create: true,
	})
	const writable = await fileHandle.createWritable()
	const payload = new Uint8Array(bytes)
	await writable.write(payload)
	await writable.close()
	return resolved.conflict
}

function deriveZipDownloadName(paths: string[]): string {
	const roots = new Set<string>()
	for (const path of paths) {
		const normalized = path.replace(/\\/g, '/')
		const [root] = normalized.split('/')
		if (root) {
			roots.add(root)
		}
	}

	if (roots.size === 1) {
		return `${[...roots][0]}.zip`
	}

	return 'download.zip'
}

async function writeCollectionToDirectory(
	root: FileSystemDirectoryHandle,
	files: ReceivedWebFile[]
): Promise<WebWriteConflict[]> {
	const conflicts: WebWriteConflict[] = []

	for (const file of files) {
		const normalized = file.name.replace(/\\/g, '/')
		const { dirHandle, fileName } = await getDirectoryForRelativePath(
			root,
			normalized
		)
		const conflict = await writeBytesToDirectory(
			dirHandle,
			fileName,
			file.bytes
		)
		if (conflict) {
			conflicts.push(conflict)
		}
	}

	return conflicts
}

export async function writeReceivedFile(
	fileName: string,
	bytes: Uint8Array
): Promise<void> {
	await writeReceivedCollection([{ name: fileName, bytes }])
}

export async function writeReceivedCollection(
	files: ReceivedWebFile[]
): Promise<WriteCollectionResult> {
	if (!files.length) {
		throw new Error('No files to save')
	}

	const isSimpleSingleFile =
		files.length === 1 &&
		!files[0].name.includes('/') &&
		!files[0].name.includes('\\')

	if (cachedDirHandle) {
		const allowed = await ensureDirWritePermission(cachedDirHandle)
		if (allowed) {
			if (isSimpleSingleFile) {
				const conflict = await writeBytesToDirectory(
					cachedDirHandle,
					basename(files[0].name),
					files[0].bytes
				)
				return {
					savedToDirectory: true,
					zippedDownload: false,
					conflicts: conflict ? [conflict] : [],
				}
			}

			const conflicts = await writeCollectionToDirectory(cachedDirHandle, files)
			return {
				savedToDirectory: true,
				zippedDownload: false,
				conflicts,
			}
		}
	}

	if (isSimpleSingleFile) {
		triggerBrowserDownload(files[0].bytes, basename(files[0].name))
		return {
			savedToDirectory: false,
			zippedDownload: false,
			conflicts: [],
		}
	}

	const zipName = deriveZipDownloadName(files.map((file) => file.name))
	const zipBytes = createStoreOnlyZip(
		files.map((file) => ({
			path: file.name,
			bytes: file.bytes,
		}))
	)
	triggerBrowserDownload(zipBytes, zipName)
	return {
		savedToDirectory: false,
		zippedDownload: true,
		conflicts: [],
	}
}
