package com.dashbeam.plugin.native_utils

import android.app.Activity
import android.app.DownloadManager
import android.app.PendingIntent
import android.content.ContentResolver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.DocumentsContract
import android.util.Log
import android.webkit.WebView
import androidx.activity.result.ActivityResult
import androidx.annotation.Keep
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.launch
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ConcurrentLinkedQueue

@InvokeArg
class SelectorArgs {
    lateinit var channel: Channel
}

@InvokeArg
class CancelJobArgs(
    var channelId: Long = 0
)

@InvokeArg
class ExportToTreeArgs {
    var treeUri: String = ""
    var sourceDir: String = ""
}

@InvokeArg
class OpenDownloadFolderArgs {
    var treeUri: String = ""
}

@InvokeArg
class ExportToMediaStoreArgs {
    var sourceDir: String = ""
}

@InvokeArg
class WriteTextToUriArgs {
    var uri: String = ""
    var contents: String = ""
}

@InvokeArg
class OpenDownloadTargetArgs {
    var uri: String = ""

    /**
     * Destination relative to external storage, e.g. `Download/DashBeam`, for
     * when there's no single file to show. Empty falls back to Downloads.
     */
    var relativePath: String = ""
}

@Keep
data class DownloadFolderSelectionResponse(
    val uri: String,
    val path: String,
)

@TauriPlugin
class NativeUtils(private val activity: Activity) : Plugin(activity) {
    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private val jobs = ConcurrentHashMap<Long, Pair<Job, String>>()
    private val pendingShareBatches = ConcurrentLinkedQueue<List<Uri>>()

    companion object {
        private const val RW_PERMISSION_FLAGS =
            Intent.FLAG_GRANT_WRITE_URI_PERMISSION or Intent.FLAG_GRANT_READ_URI_PERMISSION
        private const val SHARE_RECEIVED_EVENT = "shareReceived"
        private const val LOG_TAG = "NativeUtils"
        private const val APK_INSTALL_STATUS_ACTION =
            "com.dashbeam.plugin.native_utils.APK_INSTALL_STATUS"
        private const val APK_INSTALL_NONCE_EXTRA =
            "com.dashbeam.plugin.native_utils.APK_INSTALL_NONCE"
        private const val APK_STATUS_PREFS = "apk_install_status"
        private const val APK_STATUS_NONCE_PREFIX = "session_"
        private const val APK_PENDING_PREFS = "apk_install_pending"
        private const val APK_PENDING_URI_KEY = "pending_uri"
        private const val APK_PENDING_AT_KEY = "pending_at"
        private const val APK_PENDING_TTL_MS = 5 * 60 * 1000L

        /** Sentinel the Rust side matches to fall back to app-private staging. */
        const val MEDIA_STORE_UNSUPPORTED = "MEDIA_STORE_UNSUPPORTED"

        /** Documents provider backing the primary shared-storage volume. */
        private const val EXTERNAL_STORAGE_AUTHORITY =
            "com.android.externalstorage.documents"

        /**
         * The manifest the desktop updater already reads, kept here rather than
         * taken as an argument so this cannot become a general-purpose fetch
         * primitive for the webview. Mirrors
         * `tauri.conf.json` -> `plugins.updater.endpoints`.
         */
        private const val LATEST_MANIFEST_URL =
            "https://github.com/tonyantony300/dashbeam/releases/latest/download/latest.json"

        private const val MANIFEST_TIMEOUT_MS = 15_000

        /** Plenty for a manifest; stops a wrong URL from streaming into memory. */
        private const val MANIFEST_MAX_BYTES = 512 * 1024
    }

    @Command
    fun select_download_folder(invoke: Invoke) = startActivityForResult(
        invoke,
        Intent(Intent.ACTION_OPEN_DOCUMENT_TREE),
        this::handleDownloadFolderSelection.name
    )

    @Command
    fun select_send_document(invoke: Invoke) = startActivityForResult(
        invoke,
        Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            type = "*/*"
        },
        this::handleSendSelection.name
    )

    @Command
    fun select_send_folder(invoke: Invoke) = startActivityForResult(
        invoke,
        Intent(Intent.ACTION_OPEN_DOCUMENT_TREE),
        this::handleSendSelection.name
    )

    @Command
    fun consume_share_intent(invoke: Invoke) {
        val args = invoke.parseArgs(SelectorArgs::class.java)
        val uris = takePendingOrIntentShare()
            ?: return invoke.resolveObject(false)

        startUriCopy(uris, args.channel)
        invoke.resolveObject(true)
    }

    @Command
    fun cancel_job(invoke: Invoke) {
        val args = invoke.parseArgs(CancelJobArgs::class.java)
        val channelId = args.channelId
        val (job, tempFolder) = jobs.remove(channelId)
            ?: return invoke.reject("Trying to cancel a non existing job")
        scope.launch {
            try {
                job.cancelAndJoin()
                File(tempFolder).deleteRecursively()
                invoke.resolve()
            } catch (e: Exception) {
                invoke.reject(e.message)
            }
        }
    }

    @Command
    fun export_to_tree(invoke: Invoke) {
        val args = invoke.parseArgs(ExportToTreeArgs::class.java)
        scope.launch {
            try {
                val treeUri = Uri.parse(args.treeUri)
                val sourceDir = File(args.sourceDir)
                val result = exportDirectoryToTree(activity, treeUri, sourceDir)
                invoke.resolveObject(result)
            } catch (e: SecurityException) {
                invoke.reject(e.message ?: "SAF permission denied")
            } catch (e: Exception) {
                invoke.reject(e.message ?: "Failed to export to selected folder")
            }
        }
    }

    /**
     * Presence lifetime is decided in Rust, which knows the paired-device count
     * and the discoverability setting; these two just relay the decision.
     */
    @Command
    fun start_presence_service(invoke: Invoke) {
        PresenceService.start(activity.applicationContext)
        invoke.resolve()
    }

    @Command
    fun stop_presence_service(invoke: Invoke) {
        PresenceService.stop(activity.applicationContext)
        invoke.resolve()
    }

    @Command
    fun get_window_insets(invoke: Invoke) {
        activity.runOnUiThread {
            try {
                invoke.resolveObject(activity.readWindowInsets())
            } catch (e: Exception) {
                invoke.reject(e.message ?: "Failed to read window insets")
            }
        }
    }

    @Command
    fun open_download_folder(invoke: Invoke) {
        val args = invoke.parseArgs(OpenDownloadFolderArgs::class.java)
        val treeUriString = args.treeUri.trim()
        if (treeUriString.isEmpty()) {
            return invoke.reject("No download folder URI available")
        }

        try {
            val treeUri = Uri.parse(treeUriString)
            if (!DocumentsContract.isTreeUri(treeUri)) {
                return invoke.reject("Invalid download folder URI")
            }

            val docId = DocumentsContract.getTreeDocumentId(treeUri)
            val documentUri = DocumentsContract.buildDocumentUriUsingTree(treeUri, docId)
            val intent = Intent(Intent.ACTION_VIEW).apply {
                setDataAndType(documentUri, DocumentsContract.Document.MIME_TYPE_DIR)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            }

            try {
                activity.startActivity(intent)
            } catch (_: android.content.ActivityNotFoundException) {
                activity.startActivity(Intent.createChooser(intent, null))
            }
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject(e.message ?: "Failed to open download folder")
        }
    }

    @Command
    fun export_to_media_store(invoke: Invoke) {
        val args = invoke.parseArgs(ExportToMediaStoreArgs::class.java)
        scope.launch {
            try {
                val result = exportDirectoryToMediaStore(activity, File(args.sourceDir))
                invoke.resolveObject(result)
            } catch (_: MediaStoreUnsupportedException) {
                invoke.reject(MEDIA_STORE_UNSUPPORTED)
            } catch (e: Exception) {
                invoke.reject(e.message ?: "Failed to export to the Downloads folder")
            }
        }
    }

    /**
     * Show a received file, the folder it landed in, or the system Downloads
     * list. There's no tree URI for the SAF path and `ACTION_VIEW_DOWNLOADS`
     * only ever opens the Downloads root, so a document URI built from
     * `relativePath` is tried first — OEM file managers vary, hence the chain.
     */
    @Command
    fun open_download_target(invoke: Invoke) {
        val args = invoke.parseArgs(OpenDownloadTargetArgs::class.java)
        val uriString = args.uri.trim()
        val relativePath = args.relativePath.trim().trim('/')

        try {
            val candidates = mutableListOf<Intent>()

            if (uriString.isNotEmpty()) {
                val uri = Uri.parse(uriString)
                val mime = resolveViewMimeType(uri)
                if (isApkTarget(mime)) {
                    if (canInstallUnknownApps()) {
                        clearPendingApkInstall()
                        installApk(uri, invoke)
                        return
                    }
                    // Android kills us when the per-app install grant changes,
                    // so persist the target and resume on the next foreground.
                    storePendingApkInstall(uri)
                    if (openUnknownSourcesSettings()) {
                        invoke.reject(
                            "Grant \"Install unknown apps\" for DashBeam; the install will continue when you return"
                        )
                        return
                    }
                    clearPendingApkInstall()
                    // Fall back if the settings screen is unavailable.
                }
                candidates += Intent(Intent.ACTION_VIEW).apply {
                    setDataAndType(uri, mime)
                    addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                }
            }

            candidates += folderIntents(relativePath)
            candidates += Intent(DownloadManager.ACTION_VIEW_DOWNLOADS)

            if (!startFirstResolvable(candidates)) {
                invoke.reject("No app available to open the downloaded files")
                return
            }
            invoke.resolve()
        } catch (e: Exception) {
            invoke.reject(e.message ?: "Failed to open the downloaded file")
        }
    }

    /**
     * Write UTF-8 text to a SAF document URI. `ACTION_CREATE_DOCUMENT` (what
     * the dialog plugin's `save()` runs) creates the file and returns a
     * `content://` URI, which is not a path — `std::fs::write` on it fails and
     * leaves the freshly created 0-byte document behind.
     */
    @Command
    fun write_text_to_uri(invoke: Invoke) {
        val args = invoke.parseArgs(WriteTextToUriArgs::class.java)
        val uriString = args.uri.trim()
        if (uriString.isEmpty()) {
            return invoke.reject("No destination URI provided")
        }

        scope.launch {
            try {
                val stream = openForOverwrite(Uri.parse(uriString))
                    ?: return@launch invoke.reject("Could not open $uriString for writing")
                stream.use { it.write(args.contents.toByteArray(Charsets.UTF_8)) }
                invoke.resolve()
            } catch (e: SecurityException) {
                invoke.reject(e.message ?: "No permission to write to the selected file")
            } catch (e: Exception) {
                invoke.reject(e.message ?: "Failed to write to the selected file")
            }
        }
    }

    /**
     * Fetch the release manifest over Android's own TLS stack. Rust cannot do
     * this here: reqwest verifies through `rustls-platform-verifier`, which
     * needs a JVM handshake this app never performs. Rust parses the body.
     */
    @Command
    fun fetch_update_manifest(invoke: Invoke) {
        scope.launch {
            var connection: HttpURLConnection? = null
            try {
                connection = (URL(LATEST_MANIFEST_URL).openConnection() as HttpURLConnection).apply {
                    requestMethod = "GET"
                    connectTimeout = MANIFEST_TIMEOUT_MS
                    readTimeout = MANIFEST_TIMEOUT_MS
                    // github.com 302s release assets to another host; both
                    // legs are https, so the built-in follower handles it.
                    instanceFollowRedirects = true
                    setRequestProperty("Accept", "application/json")
                }

                val status = connection.responseCode
                if (status !in 200..299) {
                    return@launch invoke.reject("Update manifest request failed ($status)")
                }

                val body = connection.inputStream.use { readBounded(it, MANIFEST_MAX_BYTES) }

                invoke.resolveObject(body)
            } catch (e: Exception) {
                invoke.reject(e.message ?: "Could not reach the update server")
            } finally {
                connection?.disconnect()
            }
        }
    }

    /**
     * "wt" truncates, which matters when the user picks an existing file to
     * overwrite — plain "w" would leave the tail of anything longer behind.
     * Not every documents provider implements the mode, so "w" is the fallback.
     */
    private fun openForOverwrite(uri: Uri): OutputStream? {
        val resolver = activity.contentResolver
        return try {
            resolver.openOutputStream(uri, "wt")
        } catch (_: IllegalArgumentException) {
            resolver.openOutputStream(uri, "w")
        } catch (_: UnsupportedOperationException) {
            resolver.openOutputStream(uri, "w")
        }
    }

    /** Read UTF-8 up to `limit` bytes, failing rather than truncating silently. */
    private fun readBounded(input: InputStream, limit: Int): String {
        val buffer = ByteArrayOutputStream()
        val chunk = ByteArray(8 * 1024)
        while (true) {
            val read = input.read(chunk)
            if (read == -1) break
            if (buffer.size() + read > limit) {
                throw IOException("Update manifest larger than $limit bytes")
            }
            buffer.write(chunk, 0, read)
        }
        return buffer.toString(Charsets.UTF_8.name())
    }

    // Prefer the filename when the resolver returns a generic type.
    private fun resolveViewMimeType(uri: Uri): String {
        val resolverType = try {
            activity.contentResolver.getType(uri)
        } catch (_: Exception) {
            null
        }
        val displayName = try {
            resolveDisplayName(activity, uri)
        } catch (_: Exception) {
            null
        }
        if (isApkFileName(displayName) || isApkFileName(uri.lastPathSegment)) return APK_MIME_TYPE

        if (resolverType != null && !isGenericMimeType(resolverType)) return resolverType

        val candidate = displayName?.takeIf { it.isNotBlank() } ?: uri.lastPathSegment
        if (candidate != null) {
            val extMime = mimeTypeForFileName(candidate)
            if (!isGenericMimeType(extMime)) return extMime
            if (resolverType != null) return resolverType
            return extMime
        }
        return resolverType ?: "*/*"
    }

    private fun isApkTarget(mime: String): Boolean = mime == APK_MIME_TYPE

    private fun canInstallUnknownApps(): Boolean {
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            activity.packageManager.canRequestPackageInstalls()
        } else {
            true
        }
    }

    private fun openUnknownSourcesSettings(): Boolean {
        return try {
            val settingsIntent = Intent(
                android.provider.Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                Uri.parse("package:${activity.packageName}"),
            ).apply { addFlags(Intent.FLAG_ACTIVITY_NEW_TASK) }
            activity.startActivity(settingsIntent)
            true
        } catch (_: Exception) {
            false
        }
    }

    private fun apkPendingPreferences() =
        activity.getSharedPreferences(APK_PENDING_PREFS, Context.MODE_PRIVATE)

    private fun storePendingApkInstall(uri: Uri) {
        val stored = apkPendingPreferences().edit()
            .putString(APK_PENDING_URI_KEY, uri.toString())
            .putLong(APK_PENDING_AT_KEY, System.currentTimeMillis())
            .commit()
        if (!stored) Log.w(LOG_TAG, "Failed to persist the pending APK install")
    }

    private fun clearPendingApkInstall() {
        apkPendingPreferences().edit()
            .remove(APK_PENDING_URI_KEY)
            .remove(APK_PENDING_AT_KEY)
            .commit()
    }

    /**
     * Resume an install interrupted by the install-grant change. Runs from
     * onResume, or load() when cold start beats plugin setup.
     */
    private fun resumePendingApkInstall() {
        val prefs = apkPendingPreferences()
        val uriString = prefs.getString(APK_PENDING_URI_KEY, null)
        val storedAt = prefs.getLong(APK_PENDING_AT_KEY, 0L)
        if (uriString.isNullOrEmpty() || storedAt <= 0L) return

        val age = System.currentTimeMillis() - storedAt
        // Drop stale or abandoned requests instead of retrying later.
        clearPendingApkInstall()
        if (age < 0 || age > APK_PENDING_TTL_MS) return
        if (!canInstallUnknownApps()) return

        installApk(Uri.parse(uriString), null)
    }

    private fun installApk(uri: Uri, invoke: Invoke?) {
        scope.launch {
            var installer: PackageInstaller? = null
            var sessionId = -1
            var session: PackageInstaller.Session? = null
            var committed = false
            try {
                val packageInstaller = activity.packageManager.packageInstaller
                installer = packageInstaller

                val params = PackageInstaller.SessionParams(
                    PackageInstaller.SessionParams.MODE_FULL_INSTALL
                )

                sessionId = packageInstaller.createSession(params)
                val openedSession = packageInstaller.openSession(sessionId)
                session = openedSession

                activity.contentResolver.openInputStream(uri)?.use { input ->
                    openedSession.openWrite("base.apk", 0L, -1L).use { output ->
                        input.copyTo(output, BUFFER_SIZE)
                        openedSession.fsync(output)
                    }
                } ?: throw IOException("Cannot open stream for: $uri")

                val nonce = UUID.randomUUID().toString()
                storeApkStatusNonce(sessionId, nonce)

                // Activity PendingIntent keeps the app foreground for the install UI.
                val statusIntent = Intent(activity, activity.javaClass)
                    .setAction(APK_INSTALL_STATUS_ACTION)
                    .putExtra(APK_INSTALL_NONCE_EXTRA, nonce)
                val statusFlags = PendingIntent.FLAG_UPDATE_CURRENT or
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                        // Allow PackageInstaller to add the status extras.
                        PendingIntent.FLAG_MUTABLE
                    } else {
                        0
                    }
                val statusReceiver = PendingIntent.getActivity(
                    activity,
                    sessionId,
                    statusIntent,
                    statusFlags,
                )

                openedSession.commit(statusReceiver.intentSender)
                committed = true
                invoke?.resolve()
            } catch (e: Exception) {
                if (!committed) {
                    if (sessionId >= 0) {
                        try {
                            removeApkStatusNonce(sessionId)
                        } catch (_: Exception) {
                        }
                    }
                    if (session != null) {
                        try {
                            session.abandon()
                        } catch (_: Exception) {
                        }
                    } else if (sessionId >= 0) {
                        try {
                            installer?.abandonSession(sessionId)
                        } catch (_: Exception) {
                        }
                    }
                }
                invoke?.reject(e.message ?: "Failed to start the package installer")
            } finally {
                try {
                    session?.close()
                } catch (_: Exception) {
                }
            }
        }
    }

    private fun apkStatusPreferences() =
        activity.getSharedPreferences(APK_STATUS_PREFS, Context.MODE_PRIVATE)

    private fun apkStatusNonceKey(sessionId: Int) = "$APK_STATUS_NONCE_PREFIX$sessionId"

    private fun storeApkStatusNonce(sessionId: Int, nonce: String) {
        // Persist before commit: the process may die before the status arrives.
        val stored = apkStatusPreferences().edit()
            .putString(apkStatusNonceKey(sessionId), nonce)
            .commit()
        if (!stored) throw IOException("Failed to persist the APK install status")
    }

    private fun apkStatusNonce(sessionId: Int): String? =
        apkStatusPreferences().getString(apkStatusNonceKey(sessionId), null)

    private fun removeApkStatusNonce(sessionId: Int) {
        apkStatusPreferences().edit()
            .remove(apkStatusNonceKey(sessionId))
            .apply()
    }

    /** Start the installer confirmation requested by PackageInstaller. */
    private fun handleApkInstallStatus(intent: Intent?): Boolean {
        if (intent == null || intent.action != APK_INSTALL_STATUS_ACTION) return false

        val sessionId = intent.getIntExtra(PackageInstaller.EXTRA_SESSION_ID, -1)
        val nonce = intent.getStringExtra(APK_INSTALL_NONCE_EXTRA)
        val storedNonce = if (sessionId >= 0) apkStatusNonce(sessionId) else null
        if (sessionId < 0 || nonce.isNullOrEmpty() || storedNonce == null || storedNonce != nonce) {
            Log.w(LOG_TAG, "Ignoring APK install status without a valid session nonce")
            intent.action = null
            return true
        }
        // Prevent reprocessing.
        intent.action = null

        val status = intent.getIntExtra(
            PackageInstaller.EXTRA_STATUS,
            PackageInstaller.STATUS_FAILURE,
        )
        if (status != PackageInstaller.STATUS_PENDING_USER_ACTION) {
            removeApkStatusNonce(sessionId)
            val message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)
            if (status == PackageInstaller.STATUS_FAILURE) {
                Log.w(LOG_TAG, "APK install failed: $message")
            } else {
                Log.i(LOG_TAG, "APK install status $status: $message")
            }
            return true
        }

        val confirmIntent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(Intent.EXTRA_INTENT) as? Intent
        } ?: return true

        if (confirmIntent.component?.packageName == activity.packageName) {
            Log.w(LOG_TAG, "Refusing installer confirmation targeted at DashBeam")
            return true
        }

        confirmIntent.flags = Intent.FLAG_ACTIVITY_NEW_TASK
        confirmIntent.clipData = null
        try {
            activity.startActivity(confirmIntent)
        } catch (_: Exception) {
            // The session will time out.
        }
        return true
    }

    /**
     * `ACTION_VIEW` intents pointing at a storage folder, most specific first.
     * Two document-URI shapes, because file managers disagree on which they
     * accept — a bare document URI or one built against a tree.
     */
    private fun folderIntents(relativePath: String): List<Intent> {
        if (relativePath.isEmpty()) return emptyList()

        val documentId = "primary:$relativePath"
        val authority = EXTERNAL_STORAGE_AUTHORITY
        val treeUri = DocumentsContract.buildTreeDocumentUri(authority, documentId)

        return listOf(
            DocumentsContract.buildDocumentUriUsingTree(treeUri, documentId),
            DocumentsContract.buildDocumentUri(authority, documentId),
        ).map { uri ->
            Intent(Intent.ACTION_VIEW).apply {
                setDataAndType(uri, DocumentsContract.Document.MIME_TYPE_DIR)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
        }
    }

    /**
     * Fire the first intent something can handle, reporting whether any did.
     * `resolveActivity` is unreliable under API 30 package visibility, so each
     * is simply attempted in turn.
     */
    private fun startFirstResolvable(intents: List<Intent>): Boolean {
        for (intent in intents) {
            intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            try {
                activity.startActivity(intent)
                return true
            } catch (_: android.content.ActivityNotFoundException) {
                // Try the next, less specific, target.
            }
        }
        return false
    }

    @ActivityCallback
    fun handleDownloadFolderSelection(invoke: Invoke, result: ActivityResult) {
        if (Activity.RESULT_OK != result.resultCode) return invoke.resolve(null)

        val uri = result.data?.data ?: return invoke.resolve(null)

        try {
            activity.contentResolver.takePersistableUriPermission(uri, RW_PERMISSION_FLAGS)

            invoke.resolveObject(
                DownloadFolderSelectionResponse(
                    uri.toString(),
                    uri.extractFolderOsPath(),
                )
            )

            activity.contentResolver.persistedUriPermissions.stream()
                .filter { it.uri != uri }
                .forEach {
                    activity.contentResolver.releasePersistableUriPermission(
                        it.uri,
                        RW_PERMISSION_FLAGS
                    )
                }
        } catch (e: Exception) {
            invoke.reject(e.message)
        }
    }

    @ActivityCallback
    fun handleSendSelection(invoke: Invoke, result: ActivityResult) {
        val args = invoke.parseArgs(SelectorArgs::class.java)
        val channel = args.channel

        if (Activity.RESULT_OK != result.resultCode) return invoke.resolveObject(false)

        val uri = result.data?.data ?: return invoke.resolveObject(false)

        startUriCopy(listOf(uri), channel)
        invoke.resolveObject(true)
    }

    override fun load(webView: WebView) {
        super.load(webView)
        // PackageInstaller may have relaunched us with a commit result.
        val coldStartIntent = activity.intent
        webView.post {
            handleApkInstallStatus(coldStartIntent)
            // Cold start after Settings killed us for the install grant.
            resumePendingApkInstall()
        }
        // Cold start: capture share URI before / as the frontend mounts.
        // Skip wiping file_cache when a share is pending so cleanup cannot race the copy.
        val shareUris = takeShareUris(activity.intent)
        if (shareUris != null) {
            pendingShareBatches.add(shareUris)
            // Notify after the WebView can register plugin listeners (cold-start race).
            webView.post {
                trigger(SHARE_RECEIVED_EVENT, JSObject())
            }
        } else {
            scope.launch {
                activity.cacheDir.resolve("file_cache").deleteRecursively()
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        // Without this, activity.intent stays the old MAIN launcher intent under singleTask.
        activity.intent = intent

        if (handleApkInstallStatus(intent)) return

        val uris = takeShareUris(intent) ?: return
        pendingShareBatches.add(uris)
        trigger(SHARE_RECEIVED_EVENT, JSObject())
    }

    override fun onResume() {
        super.onResume()
        resumePendingApkInstall()
        // Safety net: if the frontend missed the first event (listener not ready yet),
        // re-advertise any still-unconsumed share when we come to the foreground.
        val uris = takeShareUris(activity.intent) ?: return
        pendingShareBatches.add(uris)
        trigger(SHARE_RECEIVED_EVENT, JSObject())
    }

    override fun onDestroy() {
        jobs.forEach { _, (job, tempFolder) ->
            try {
                job.cancel()
                File(tempFolder).deleteRecursively()
            } catch (_: Exception) {
            }
        }

        scope.cancel()
        super.onDestroy()
    }

    private fun startUriCopy(uris: List<Uri>, channel: Channel) {
        val path = listOf(
            activity.cacheDir.absolutePath,
            "file_cache",
            System.currentTimeMillis().toString(),
        ).joinToString(File.separator)

        val tempFolder = File(path)
        val job = scope.launch(start = CoroutineStart.LAZY) {
            try {
                tempFolder.parentFile?.mkdirs()
                    ?: throw IOException("Unable to create parent folders for ${tempFolder.absolutePath}")

                if (uris.size == 1) {
                    copyUri(activity, uris.single(), tempFolder).collect {
                        channel.send(it.toJSObject())
                    }
                } else {
                    val totalBytes = uris.sumOf { resolveContentLength(activity, it) }
                    channel.send(CopyProgress(0, totalBytes, tempFolder.absolutePath).toJSObject())

                    var copiedBytes = 0L
                    val cachedPaths = mutableListOf<String>()
                    uris.forEachIndexed { index, uri ->
                        var fileBytes = 0L
                        copyUri(activity, uri, tempFolder.resolve(index.toString())).collect { progress ->
                            fileBytes = progress.copiedBytes
                            progress.cachedPath?.takeIf { progress.completed }?.let(cachedPaths::add)
                            channel.send(
                                CopyProgress(
                                    copiedBytes + progress.copiedBytes,
                                    totalBytes,
                                    null,
                                ).toJSObject()
                            )
                        }
                        copiedBytes += fileBytes
                    }

                    channel.send(
                        CopyProgress(
                            copiedBytes,
                            if (totalBytes > 0) totalBytes else copiedBytes,
                            null,
                            cachedPaths,
                            completed = true,
                        ).toJSObject()
                    )
                }
            } catch (e: Exception) {
                tempFolder.deleteRecursively()
                channel.send(
                    JSObject().apply {
                        put("error", e.message ?: "Failed to copy shared file")
                        put("progress", -1.0)
                        put("copiedBytes", "0")
                        put("totalBytes", "0")
                    }
                )
            } finally {
                jobs.remove(channel.id)
            }
        }

        jobs[channel.id] = job to tempFolder.absolutePath
        job.start()
    }

    @Synchronized
    private fun takePendingOrIntentShare(): List<Uri>? {
        return pendingShareBatches.poll() ?: takeShareUris(activity.intent)
    }

    private fun peekShareUris(intent: Intent?): List<Uri>? {
        if (intent == null ||
            (intent.action != Intent.ACTION_SEND && intent.action != Intent.ACTION_SEND_MULTIPLE)
        ) {
            return null
        }
        return extractShareUris(intent).takeIf { it.isNotEmpty() }
    }

    private fun takeShareUris(intent: Intent?): List<Uri>? {
        val uris = peekShareUris(intent) ?: return null
        intent?.action = null
        return uris
    }

    private fun extractShareUris(intent: Intent): List<Uri> {
        val uris = mutableListOf<Uri>()
        parcelableStreamExtras(intent)?.let(uris::addAll)

        when (val stream = intent.extras?.get(Intent.EXTRA_STREAM)) {
            is Uri -> uris.add(stream)
            is String -> if (stream.isNotBlank()) uris.add(Uri.parse(stream))
            is List<*> -> uris.addAll(stream.filterIsInstance<Uri>())
        }

        val clip = intent.clipData
        if (clip != null) {
            repeat(clip.itemCount) { index ->
                clip.getItemAt(index)?.uri?.let(uris::add)
            }
        }

        intent.data?.let(uris::add)

        return uris.distinct().filter { it.scheme == ContentResolver.SCHEME_CONTENT }
    }

    @Suppress("DEPRECATION")
    private fun parcelableStreamExtras(intent: Intent): List<Uri>? {
        return if (intent.action == Intent.ACTION_SEND_MULTIPLE) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
            } else {
                intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM)
            }
        } else {
            val uri = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
            } else {
                intent.getParcelableExtra(Intent.EXTRA_STREAM) as? Uri
            }
            uri?.let(::listOf)
        }
    }
}

fun Uri.extractFolderOsPath(): String {
    require(DocumentsContract.isTreeUri(this))

    val path = this.path
        ?: throw IOException("Unable to get path from selected download folder uri: $this")
    val baseExternalPath = Environment.getExternalStorageDirectory().path
    return try {
        val docId = DocumentsContract.getTreeDocumentId(this)
        val segments = docId.split(":")
        when {
            "primary" == segments[0] && segments.size > 1 -> "${baseExternalPath}/${segments[1]}"
            "primary" == segments[0] -> baseExternalPath
            segments.size > 1 -> "/storage/${segments[0]}/${segments[1]}"
            else -> "/storage/${segments[0]}/"
        }
    } catch (_: Exception) {
        path
    }
}
