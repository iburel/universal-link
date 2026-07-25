package org.universallink.mobile

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)

        // Android 13+: the foreground-service notification needs this runtime
        // permission to be VISIBLE. The service runs either way, but ask so the
        // ongoing notification (and thus the reason the app stays active) shows.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
            != PackageManager.PERMISSION_GRANTED
        ) {
            ActivityCompat.requestPermissions(
                this,
                arrayOf(Manifest.permission.POST_NOTIFICATIONS),
                POST_NOTIFICATIONS_REQUEST,
            )
        }

        // Keep the process (and the embedded Core's network) alive while the app
        // is backgrounded — see UlForegroundService. Guarded so a start failure
        // never takes the UI down with it.
        try {
            UlForegroundService.start(this)
        } catch (t: Throwable) {
            android.util.Log.e("ULCore", "failed to start the foreground service", t)
        }

        // A share that cold-started the app. Only on a FRESH start: after a
        // recreation (rotation, process restore) getIntent() still holds the
        // original share, which the previous instance already consumed.
        if (savedInstanceState == null) {
            handleShare(intent)
        }
    }

    override fun onNewIntent(intent: Intent) {
        // Keep tao's and the plugins' dispatch alive first. tao also turns an
        // ACTION_SEND text/plain into a RunEvent::Opened, which the Rust side
        // deliberately ignores: it wraps the text in a data: URL and normalizes
        // anything URL-shaped, while this seam carries it byte-exact.
        super.onNewIntent(intent)
        handleShare(intent)
    }

    /** A share → the embedded Core (see ShareBridge / ShareFiles / share.rs). */
    private fun handleShare(intent: Intent?) {
        if (intent == null) return
        val action = intent.action
        if (action != Intent.ACTION_SEND && action != Intent.ACTION_SEND_MULTIPLE) return
        // Relaunched from Recents: the system replays the ORIGINAL intent, and
        // the action we null below was only nulled in this process's copy — so a
        // process death would otherwise re-share what the user shared once.
        if (intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY != 0) return

        // Text or files? The two are told apart HERE, once, because each side's
        // answer depends on the other's: a stream wins over text (a plain-text
        // FILE is shared as text/plain), while text wins over a bare ClipData URI
        // (a shared link carries a preview thumbnail there — see ShareFiles).
        val text = sharedText(intent)
        var taken = false
        try {
            taken = ShareFiles.handle(this, intent, text != null)
        } catch (t: Throwable) {
            android.util.Log.e("ULCore", "failed to hand the shared files to the core", t)
        }
        if (!taken && action == Intent.ACTION_SEND && text != null) {
            taken = shareText(intent, text)
        }
        // Consume it: a later recreation must not share the same thing twice.
        if (taken) {
            intent.action = null
            setIntent(intent)
        }
    }

    /**
     * The text a share carries, or null. EXTRA_TEXT is where a text share puts it;
     * some apps fill only the ClipData, whose FIRST item is then the text itself
     * (a preview thumbnail is a URI item, and has no text).
     */
    private fun sharedText(intent: Intent): String? =
        (intent.getStringExtra(Intent.EXTRA_TEXT)
            ?: intent.clipData?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString())
            ?.takeIf { it.isNotEmpty() }

    /** Shared text → the account's clipboard. Returns whether it took the intent. */
    private fun shareText(intent: Intent, text: String): Boolean {
        // A share that is not text/* but carries text (a caption, a subject) is not
        // a text share: its payload is elsewhere, and we would send the caption.
        if (intent.type?.startsWith("text/") != true) return false
        try {
            ShareBridge.onShareText(text)
        } catch (t: Throwable) {
            android.util.Log.e("ULCore", "failed to hand the shared text to the core", t)
            return false
        }
        return true
    }

    override fun onDestroy() {
        try {
            UlForegroundService.stop(this)
        } catch (_: Throwable) {
        }
        super.onDestroy()
    }

    companion object {
        private const val POST_NOTIFICATIONS_REQUEST = 1
    }
}
