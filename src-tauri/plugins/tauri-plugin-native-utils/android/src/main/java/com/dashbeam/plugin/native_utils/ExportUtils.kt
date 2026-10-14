package com.dashbeam.plugin.native_utils

import android.content.Context
import android.net.Uri
import androidx.annotation.Keep
import androidx.documentfile.provider.DocumentFile
import java.io.File
import java.io.IOException

@Keep
data class ExportConflict(
    val original: String,
    val resolved: String,
)

@Keep
data class ExportToTreeResult(
    val exportedCount: Int,
    val conflicts: List<ExportConflict>,
)

fun exportDirectoryToTree(
    context: Context,
    treeUri: Uri,
    sourceDir: File,
): ExportToTreeResult {
    if (!sourceDir.exists() || !sourceDir.isDirectory) {
        throw IOException("Source directory does not exist: ${sourceDir.absolutePath}")
    }

    val root = DocumentFile.fromTreeUri(context, treeUri)
        ?: throw IOException("Cannot open tree URI: $treeUri")

    if (!root.canWrite()) {
        throw IOException("Tree URI is not writable: $treeUri")
    }

    val conflicts = mutableListOf<ExportConflict>()
    var exportedCount = 0

    val files = sourceDir.walkTopDown().filter { it.isFile }.toList()
    for (file in files) {
        val relative = file.relativeTo(sourceDir).invariantSeparatorsPath
        if (relative.isBlank()) continue

        val parts = relative.split('/').filter { it.isNotEmpty() }
        if (parts.isEmpty()) continue

        var parent = root
        for (dirName in parts.dropLast(1)) {
            parent = ensureDirectory(parent, dirName)
        }

        val fileName = parts.last()
        val desiredRelative = relative
        val (targetName, resolvedRelative) = resolveConflictName(parent, fileName)
        if (resolvedRelative != null) {
            val conflictRelative = parts.dropLast(1).let { dirs ->
                if (dirs.isEmpty()) resolvedRelative else (dirs + resolvedRelative).joinToString("/")
            }
            conflicts.add(
                ExportConflict(
                    original = desiredRelative,
                    resolved = conflictRelative,
                )
            )
        }

        val mime = mimeTypeForFileName(targetName)
        val created = parent.createFile(mime, targetName)
            ?: throw IOException("Failed to create file '$targetName' under $treeUri")

        context.contentResolver.openOutputStream(created.uri)?.use { output ->
            file.inputStream().use { input ->
                val buffer = ByteArray(BUFFER_SIZE)
                var bytesRead: Int
                while (input.read(buffer).also { bytesRead = it } != -1) {
                    output.write(buffer, 0, bytesRead)
                }
                output.flush()
            }
        } ?: throw IOException("Cannot open output stream for: ${created.uri}")

        exportedCount += 1
    }

    return ExportToTreeResult(
        exportedCount = exportedCount,
        conflicts = conflicts,
    )
}

private fun ensureDirectory(parent: DocumentFile, name: String): DocumentFile {
    parent.findFile(name)?.let { existing ->
        if (existing.isDirectory) return existing
        throw IOException("Path component exists as a file: $name")
    }
    return parent.createDirectory(name)
        ?: throw IOException("Failed to create directory: $name")
}

/**
 * Returns Pair(targetDisplayNameForCreateFile, resolvedRelativeNameOrNull).
 * DocumentFile.createFile may strip extensions from displayName when MIME includes it —
 * we pass the full filename and application/octet-stream for unknown types.
 */
private fun resolveConflictName(
    parent: DocumentFile,
    fileName: String,
): Pair<String, String?> {
    if (parent.findFile(fileName) == null) {
        return fileName to null
    }

    val dot = fileName.lastIndexOf('.')
    val stem: String
    val extension: String?
    if (dot > 0) {
        stem = fileName.substring(0, dot)
        extension = fileName.substring(dot + 1)
    } else {
        stem = fileName
        extension = null
    }

    for (index in 1 until 10_000) {
        val candidate = if (extension != null) {
            "$stem ($index).$extension"
        } else {
            "$fileName ($index)"
        }
        if (parent.findFile(candidate) == null) {
            return candidate to candidate
        }
    }

    throw IOException("Too many filename conflicts for $fileName")
}
