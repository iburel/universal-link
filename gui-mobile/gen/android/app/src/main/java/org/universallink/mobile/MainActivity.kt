package org.universallink.mobile

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
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
     * Two things had to be learnt on the device for this to work at all:
     * - the insets are read off the WINDOW, not off the dispatch chain, because
     *   AppCompat's decor (`FitWindowsLinearLayout`, `fitsSystemWindows=true`)
     *   consumes the system-window insets before any listener here sees them;
     * - and they go on the content view rather than on the webview, whose own
     *   padding it renders straight through (padding applied, nothing moved).
     *
     * Driven by layout so it follows a rotation, and idempotent so the layout that
     * `setPadding` itself triggers cannot loop.
     */
    private fun insetContentFromSystemBars() {
        val content = findViewById<View>(android.R.id.content) ?: return
        val fit = {
            val safe = ViewCompat.getRootWindowInsets(content)?.getInsets(
                WindowInsetsCompat.Type.systemBars() or
                    WindowInsetsCompat.Type.displayCutout(),
            )
            if (safe != null &&
                (content.paddingLeft != safe.left || content.paddingTop != safe.top ||
                    content.paddingRight != safe.right || content.paddingBottom != safe.bottom)
            ) {
                Log.i(TAG, "insetting the content view: $safe")
                content.setPadding(safe.left, safe.top, safe.right, safe.bottom)
            }
        }
        content.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> fit() }
        fit()
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
