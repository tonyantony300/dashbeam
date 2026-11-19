package com.dashbeam.plugin.native_utils

import android.content.ContentResolver
import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import androidx.annotation.Keep
import androidx.annotation.RequiresApi
import java.io.File
import java.io.IOException

/** Public folder receives land in when the user has not picked one of their own. */
const val MEDIA_STORE_SUBDIR = "DashBeam"

/**
 * Thrown when the device predates scoped storage. `RELATIVE_PATH` arrived in
 * API 29; below it, writing under `Download/` needs a broad storage permission,
 * so the caller falls back to app-private staging.
 */
class MediaStoreUnsupportedException : IOException("MediaStore exports require Android 10")

@Keep
data class MediaStoreExportResult(
    val exportedCount: Int,
    val conflicts: List<ExportConflict>,
    val uris: List<String>,
    val displayPath: String,
)

/**
 * Copy a staging directory into the public `Download/DashBeam` collection. On
 * API 29+ this needs no storage permission at all, which is what makes it
 * viable as a zero-prompt default — writing fresh files never does, though
 * re-reading them after a reinstall would.
 */
fun exportDirectoryToMediaStore(
    context: Context,
    sourceDir: File,
): MediaStoreExportResult {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
        throw MediaStoreUnsupportedException()
    }

    return exportToDownloads(context, sourceDir)
}

@RequiresApi(Build.VERSION_CODES.Q)
private fun exportToDownloads(
    context: Context,
    sourceDir: File,
): MediaStoreExportResult {
    if (!sourceDir.exists() || !sourceDir.isDirectory) {
        throw IOException("Source directory does not exist: ${sourceDir.absolutePath}")
    }

    val resolver = context.contentResolver
    val collection = MediaStore.Downloads.EXTERNAL_CONTENT_URI
    val conflicts = mutableListOf<ExportConflict>()
    val uris = mutableListOf<String>()
    var exportedCount = 0

    val files = sourceDir.walkTopDown().filter { it.isFile }.toList()
    for (file in files) {
        val relative = file.relativeTo(sourceDir).invariantSeparatorsPath
        if (relative.isBlank()) continue

        val parts = relative.split('/').filter { it.isNotEmpty() }
        if (parts.isEmpty()) continue

        val fileName = parts.last()
        // Nested transfers keep their shape: a collection's own subdirectories
        // become subdirectories of Download/DashBeam.
        val relativePath = buildRelativePath(parts.dropLast(1))

        val pending = ContentValues().apply {
            put(MediaStore.Downloads.DISPLAY_NAME, fileName)
            put(MediaStore.Downloads.MIME_TYPE, mimeTypeForFileName(fileName))
            put(MediaStore.Downloads.RELATIVE_PATH, relativePath)
            put(MediaStore.Downloads.IS_PENDING, 1)
        }

        val uri = resolver.insert(collection, pending)
            ?: throw IOException("Failed to create '$fileName' under $relativePath")

        try {
            resolver.openOutputStream(uri)?.use { output ->
                file.inputStream().use { input ->
                    input.copyTo(output, BUFFER_SIZE)
                }
            } ?: throw IOException("Cannot open output stream for: $uri")

            // Clearing IS_PENDING publishes the row; until then the file is
            // invisible to other apps, so a crash mid-copy cannot leave a
            // half-written file on show in the Files app.
            resolver.update(
                uri,
                ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) },
                null,
                null,
            )
        } catch (e: Exception) {
            resolver.delete(uri, null, null)
            throw e
        }

        // MediaStore renames rather than overwrites when a name is taken, so
        // the row's final name is the only way to learn a conflict happened.
        val actualName = resolver.queryDisplayName(uri)
        if (actualName != null && actualName != fileName) {
            val dirs = parts.dropLast(1)
            conflicts.add(
                ExportConflict(
                    original = relative,
                    resolved = (dirs + actualName).joinToString("/"),
                )
            )
        }

        uris.add(uri.toString())
        exportedCount += 1
    }

    return MediaStoreExportResult(
        exportedCount = exportedCount,
        conflicts = conflicts,
        uris = uris,
        displayPath = buildRelativePath(emptyList()).trimEnd('/'),
    )
}

private fun buildRelativePath(subDirs: List<String>): String {
    val segments = listOf(Environment.DIRECTORY_DOWNLOADS, MEDIA_STORE_SUBDIR) + subDirs
    return segments.joinToString("/") + "/"
}

@RequiresApi(Build.VERSION_CODES.Q)
private fun ContentResolver.queryDisplayName(uri: Uri): String? =
    query(uri, arrayOf(MediaStore.Downloads.DISPLAY_NAME), null, null, null)?.use { cursor ->
        if (cursor.moveToFirst()) cursor.getString(0) else null
    }
