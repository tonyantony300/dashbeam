package com.dashbeam.plugin.native_utils

import android.webkit.MimeTypeMap

const val APK_MIME_TYPE = "application/vnd.android.package-archive"
const val GENERIC_MIME_TYPE = "application/octet-stream"

fun isApkFileName(fileName: String?): Boolean =
    fileName?.trim()?.endsWith(".apk", ignoreCase = true) == true

fun mimeTypeForFileName(fileName: String): String {
    val trimmed = fileName.trim().substringAfterLast('/').trim()
    // Some OEM MIME maps miss .apk.
    if (isApkFileName(trimmed)) return APK_MIME_TYPE

    val dot = trimmed.lastIndexOf('.')
    if (dot < 0 || dot == trimmed.length - 1) return GENERIC_MIME_TYPE

    val ext = trimmed.substring(dot + 1).lowercase()
    if (ext.any { !it.isLetterOrDigit() }) return GENERIC_MIME_TYPE

    return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext)
        ?: GENERIC_MIME_TYPE
}

fun isGenericMimeType(mime: String?): Boolean =
    mime == null || mime == "*/*" || mime == GENERIC_MIME_TYPE
