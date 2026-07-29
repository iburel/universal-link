package org.universallink.mobile

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // BEFORE super.onCreate: that is what boots the embedded Core, and the
        // Core must not be able to report work before the seam that carries it
        // exists (see KeepAlive). It needs nothing from Tauri — it loads the
        // library itself — so it can run this early.
        KeepAlive.attach(this)

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

        // The window a scanner is opened from, and the answer to "can this device
        // read a code at all". After super.onCreate: unlike the keep-alive seam,
        // nothing can ask for a scan before the page that offers the button exists.
        ScanBridge.attach(this)

        insetContentFromSystemBars()

        // A share that cold-started the app. Only on a FRESH start: after a
        // recreation (rotation, process restore) getIntent() still holds the
        // original share, which the previous instance already consumed.
        if (savedInstanceState == null) {
            handleShare(intent)
        }
    }

    /**
     * Keeps the page out from under the system bars.
     *
     * The window is edge-to-edge — enforced from Android 15 on, so not ours to opt
     * out of — which means the webview covers the status bar and the gesture bar.
     * The page is not told about it (in an Android webview `env(safe-area-inset-*)`
     * reports display CUTOUTS only, so a bottom tab bar would still sit under the
     * gesture bar on any phone without a notch): the CONTENT VIEW is inset instead,
     * which shrinks the webview wry put in it (`setContentView`) and with it the
     * page's viewport. What is left over shows the window background (the DayNight
     * theme's), within a shade of the page's own — the page never draws there.
     *
     * Three things had to be learnt on the device for this to work at all:
     * - a listener on the WEBVIEW never fires, because AppCompat's decor
     *   (`FitWindowsLinearLayout`, `fitsSystemWindows=true`) consumes the
     *   system-window insets before anything below it is asked;
     * - the padding goes on the content view rather than on the webview, whose own
     *   padding it renders straight through (padding applied, nothing moved);
     * - and the values must come from the WINDOW's own dispatch, at the top of the
     *   chain where nothing has consumed them yet, NOT from polling
     *   `getRootWindowInsets` off a layout pass. That last one is what broke
     *   landscape: after a rotation (the activity is not recreated —
     *   `android:configChanges` covers orientation) a layout pass can still read
     *   the PRE-rotation insets, and since the only guard here is "does this differ
     *   from the current padding", the last read wins with nothing to correct it.
     *   The result was portrait-shaped padding on a landscape window: the page ran
     *   under the left cutout and under the right-hand navigation bar (the sidebar
     *   labels came out as "evices", "pprovals"), with a dead 132px band along the
     *   bottom.
     *
     * So the decor view's `onApplyWindowInsets` is the source of truth, and it
     * cannot be stale: the framework hands us the new insets. The layout pass still
     * re-applies them, because a layout can happen without a dispatch (the webview
     * arriving, the keyboard, a resize), but it replays the REMEMBERED value rather
     * than re-reading — so once a dispatch has been seen, the polling path that
     * caused the bug is gone for good. Polling remains only to seed the very first
     * layout, which may precede any dispatch.
     *
     * Idempotent by comparison, so the layout that `setPadding` itself triggers
     * cannot loop.
     */
    private fun insetContentFromSystemBars() {
        val content = findViewById<View>(android.R.id.content) ?: return
        // The last insets the framework handed us. Null until the first dispatch.
        var dispatched: Insets? = null
        val apply = { safe: Insets ->
            if (content.paddingLeft != safe.left || content.paddingTop != safe.top ||
                content.paddingRight != safe.right || content.paddingBottom != safe.bottom
            ) {
                Log.i(TAG, "insetting the content view: $safe")
                content.setPadding(safe.left, safe.top, safe.right, safe.bottom)
            }
        }
        val safeOf = { insets: WindowInsetsCompat ->
            insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout(),
            )
        }

        // Observe at the decor view, above AppCompat's decor, and pass the insets on
        // to the default implementation — this watches the dispatch, it does not
        // intercept it.
        ViewCompat.setOnApplyWindowInsetsListener(window.decorView) { v, insets ->
            dispatched = safeOf(insets)
            dispatched?.let(apply)
            ViewCompat.onApplyWindowInsets(v, insets)
        }
        content.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ ->
            val safe = dispatched ?: ViewCompat.getRootWindowInsets(content)?.let(safeOf)
            safe?.let(apply)
        }
        ViewCompat.getRootWindowInsets(content)?.let(safeOf)?.let(apply)
    }

    /**
     * A rotation does not recreate this activity (`android:configChanges`), so ask
     * for a fresh inset dispatch rather than waiting to be told — see
     * [insetContentFromSystemBars] for what went wrong when the padding was left to
     * whatever a post-rotation layout pass happened to read.
     */
    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        findViewById<View>(android.R.id.content)?.let { ViewCompat.requestApplyInsets(it) }
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
            Log.e(TAG, "failed to hand the shared files to the core", t)
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
            Log.e(TAG, "failed to hand the shared text to the core", t)
            return false
        }
        return true
    }

    // No onDestroy override: the foreground service is no longer this activity's
    // to stop. It follows the work (KeepAlive), which is the whole point — a
    // transfer must survive the window going away.

    companion object {
        private const val POST_NOTIFICATIONS_REQUEST = 1
        private const val TAG = "ULCore"
    }
}
