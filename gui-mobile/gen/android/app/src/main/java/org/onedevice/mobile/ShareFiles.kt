package org.onedevice.mobile

import android.content.ContentResolver
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Log
import androidx.core.content.IntentCompat
import java.io.File
import java.io.FileOutputStream
import java.util.UUID
import java.util.concurrent.Executors
import org.json.JSONArray
import org.json.JSONObject

/**
 * Files shared from the OS share sheet, copied into the app's cache for the
 * embedded Core to send.
 *
 * **Why a copy.** A share hands us `content://` URIs. Those are not paths: they
 * are a permission grant, made to this process, over a provider that may hold
 * the bytes anywhere (another app's private storage, the cloud). Nothing outside
 * a ContentResolver can read them — and the Core needs to read them LATER, after
 * the user has chosen a destination, potentially minutes later and certainly
 * after the grant has lapsed with the activity. Copying now is the price of
 * scoped storage, not a shortcut; a lazier design (copy once a destination is
 * picked) would have to keep the grant alive across the pick, which Android does
 * not promise.
 *
 * The copy is also where a shared file gets its NAME: the receiving device names
 * what it receives after the basename of the path it was sent (the Core's
 * manifest), so a copy at `…/shares/<id>/holiday.jpg` arrives as `holiday.jpg`.
 *
 * The Rust side (gui-mobile/src/share.rs) owns the lifecycle of these
 * directories from the moment a share is reported: it adopts one per share and
 * drops it when the frontend is done with it.
 */
object ShareFiles {
    private const val TAG = "ULCore"

    /** One directory per share, under the app's cache. */
    private const val ROOT = "shares"

    /** Display names are truncated to this (the tail: it keeps the extension). */
    private const val MAX_NAME = 120

    /**
     * Room left free in the cache when checking whether a share fits. Filling
     * internal storage hurts the whole phone; refusing one share does not.
     */
    private const val SPACE_MARGIN = 32L * 1024 * 1024

    /**
     * Copies run ONE AT A TIME, and off the UI thread.
     *
     * The single thread is not just tidiness: the Rust side treats every directory
     * under the cache root it does not know about as a leftover and deletes it
     * (share.rs `register`), and a directory is only made known once its copy is
     * COMPLETE. Two copies at once would therefore have the faster one delete the
     * slower one's directory mid-write — and on Linux that unlink succeeds
     * silently, so the slow copy would finish into a file nobody can open and
     * report a share whose paths do not exist. Serializing keeps that invariant
     * true instead of weakening the sweep.
     */
    private val worker = Executors.newSingleThreadExecutor { r -> Thread(r, "1device-share-copy") }

    /**
     * Takes over `intent` if it carries files, reporting progress through
     * [ShareBridge]. Returns false if it carries none — the caller then tries the
     * text path. Only reads the intent; the copying is queued.
     *
     * `hasText` is the caller's verdict on whether this share carries text (see
     * MainActivity.handleShare), which decides whether a bare ClipData URI counts
     * as the payload — see [streams].
     */
    fun handle(context: Context, intent: Intent, hasText: Boolean): Boolean {
        val uris = streams(intent, hasText)
        if (uris.isEmpty()) return false
        // The URI grant belongs to the process, so the worker can read it as long
        // as this activity lives — and it does: it is showing the picker.
        val app = context.applicationContext
        worker.execute {
            report(JSONObject().put("phase", "preparing").put("files", uris.size))
            copy(app.contentResolver, app.cacheDir, uris)
        }
        return true
    }

    /** The stream URIs of a share, in the order the sender listed them. */
    private fun streams(intent: Intent, hasText: Boolean): List<Uri> {
        val uris = ArrayList<Uri>()
        when (intent.action) {
            Intent.ACTION_SEND ->
                IntentCompat
                    .getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java)
                    ?.let { uris.add(it) }
            Intent.ACTION_SEND_MULTIPLE ->
                IntentCompat
                    .getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java)
                    ?.filterNotNullTo(uris)
            else -> return emptyList()
        }
        // Some apps put the stream ONLY in the ClipData. But a text share puts its
        // share-sheet PREVIEW there — Chrome attaches a thumbnail URI to every link
        // it shares — so a ClipData URI is the payload only when there is no text
        // to share instead. Otherwise sharing a link would send a screenshot of the
        // page and drop the URL. An explicit EXTRA_STREAM always wins: that is how
        // a file share is documented to travel, text alongside it or not.
        if (uris.isEmpty() && !hasText) {
            val clip = intent.clipData
            if (clip != null) {
                for (i in 0 until clip.itemCount) clip.getItemAt(i).uri?.let { uris.add(it) }
            }
        }
        return uris
    }

    private fun copy(resolver: ContentResolver, cache: File, uris: List<Uri>) {
        val dir = File(File(cache, ROOT), UUID.randomUUID().toString())
        if (!dir.mkdirs()) {
            Log.e(TAG, "could not create the share directory ${dir.absolutePath}")
            fail("unreadable")
            return
        }
        val files = JSONArray()
        try {
            if (!fits(resolver, dir, uris)) {
                dir.deleteRecursively()
                fail("no_space")
                return
            }
            uris.forEachIndexed { index, uri ->
                val out = unique(dir, name(resolver, uri, index))
                val written = resolver.openInputStream(uri).use { input ->
                    requireNotNull(input) { "no stream for $uri" }
                    FileOutputStream(out).use { input.copyTo(it) }
                }
                files.put(
                    JSONObject()
                        .put("path", out.absolutePath)
                        .put("name", out.name)
                        .put("size", written),
                )
            }
        } catch (t: Throwable) {
            Log.e(TAG, "could not copy the shared files", t)
            dir.deleteRecursively()
            fail("unreadable")
            return
        }
        if (files.length() == 0) {
            dir.deleteRecursively()
            fail("unreadable")
            return
        }
        report(
            JSONObject()
                .put("phase", "pick")
                .put("dir", dir.absolutePath)
                .put("files", files),
        )
    }

    /**
     * Whether the share fits in the cache. A provider is free not to publish a
     * size, and half of one is enough to make this pointless — so an unknown size
     * means "go ahead": a copy that fails half-way is reported all the same, just
     * less precisely.
     */
    private fun fits(resolver: ContentResolver, dir: File, uris: List<Uri>): Boolean {
        var total = 0L
        for (uri in uris) {
            total += queryLong(resolver, uri, OpenableColumns.SIZE) ?: return true
        }
        return total + SPACE_MARGIN < dir.usableSpace
    }

    private fun name(resolver: ContentResolver, uri: Uri, index: Int): String {
        val raw = queryString(resolver, uri, OpenableColumns.DISPLAY_NAME) ?: uri.lastPathSegment
        return sanitize(raw) ?: "shared-${index + 1}"
    }

    /**
     * A display name comes from another app, and becomes both the basename of a
     * path we build here and the name the receiving device sees. Anything that
     * could climb out of the share directory is refused rather than repaired —
     * the caller then falls back to a name of our own.
     */
    private fun sanitize(raw: String?): String? {
        val base = raw?.substringAfterLast('/')?.substringAfterLast('\\')?.trim() ?: return null
        if (base.isEmpty() || base == "." || base == "..") return null
        if (base.any { it.code < 0x20 }) return null
        return if (base.length > MAX_NAME) base.takeLast(MAX_NAME) else base
    }

    /** Two shared files can carry the same name: the second gets a suffix. */
    private fun unique(dir: File, name: String): File {
        var candidate = File(dir, name)
        var n = 2
        while (candidate.exists()) {
            val dot = name.lastIndexOf('.')
            val stem = if (dot > 0) name.substring(0, dot) else name
            val ext = if (dot > 0) name.substring(dot) else ""
            candidate = File(dir, "$stem-$n$ext")
            n++
        }
        return candidate
    }

    private fun queryString(resolver: ContentResolver, uri: Uri, column: String): String? =
        query(resolver, uri, column) { c -> c.getString(0) }

    private fun queryLong(resolver: ContentResolver, uri: Uri, column: String): Long? =
        query(resolver, uri, column) { c -> c.getLong(0) }

    /**
     * Reads one column of a provider's metadata. A provider that does not
     * implement `query` (a plain `file://` URI, for one) throws or returns
     * nothing: both mean "unknown", never a failed share.
     */
    private fun <T> query(
        resolver: ContentResolver,
        uri: Uri,
        column: String,
        read: (android.database.Cursor) -> T,
    ): T? = try {
        resolver.query(uri, arrayOf(column), null, null, null)?.use { cursor ->
            if (cursor.moveToFirst() && !cursor.isNull(0)) read(cursor) else null
        }
    } catch (t: Throwable) {
        Log.w(TAG, "no $column for $uri", t)
        null
    }

    private fun fail(reason: String) {
        report(JSONObject().put("phase", "failed").put("reason", reason))
    }

    private fun report(status: JSONObject) {
        try {
            ShareBridge.onShareFiles(status.toString())
        } catch (t: Throwable) {
            Log.e(TAG, "failed to hand a file share to the core", t)
        }
    }
}
