package com.dashbeam.plugin.native_utils

import android.webkit.MimeTypeMap

const val APK_MIME_TYPE = "application/vnd.android.package-archive"
const val GENERIC_MIME_TYPE = "application/octet-stream"

fun mimeTypeForFileName(fileName: String): String {
    val trimmed = fileName.trim().substringAfterLast('/').trim()
    // MimeTypeMap is sparse on some OEM builds, so check apk directly.
    if (trimmed.endsWith(".apk", ignoreCase = true)) return APK_MIME_TYPE

    val dot = trimmed.lastIndexOf('.')
    if (dot < 0 || dot == trimmed.length - 1) return GENERIC_MIME_TYPE

    val ext = trimmed.substring(dot + 1).lowercase()
    // Providers may append an extension matching the MIME type, so require an exact one.
    if (ext.any { !it.isLetterOrDigit() }) return GENERIC_MIME_TYPE

    return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext)
        ?: GENERIC_MIME_TYPE
}

fun isGenericMimeType(mime: String?): Boolean =
    mime == null || mime == "*/*" || mime == GENERIC_MIME_TYPE
