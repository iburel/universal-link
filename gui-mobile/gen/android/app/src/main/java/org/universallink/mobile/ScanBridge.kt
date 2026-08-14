package org.universallink.mobile

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.lang.ref.WeakReference
import org.json.JSONObject

/**
 * The seam for reading a pairing code with the camera. It goes BOTH ways, unlike
 * the other two: Rust asks for a scan (only Kotlin can put a camera on screen),
 * and Kotlin reports its outcome (only Rust can answer the frontend's command).
 * The Rust side is `gui-mobile/src/scan.rs`.
 *
 * The JNI names are derived from this package, this object and the method names,
 * so renaming any of them breaks the link at RUNTIME, not at build time — and R8
 * is entitled to delete [startScan] outright, which is why app/proguard-rules.pro
 * keeps it by name (that exact failure has already been seen here, with
 * `KeepAlive.setWork`).
 *
 * # Why the scanner is ours
 *
 * The one-line alternative is `com.google.android.gms:play-services-code-scanner`:
 * no camera permission at all, since the UI runs inside Google Play Services and
 * only a string comes back. It was not taken, for three reasons that all matter
 * here:
 *
 * - it is proprietary, in an AGPL-3.0 app;
 * - it needs Google Play Services, so the app would have no scanner at all on a
 *   de-Googled Android — while the whole point of this project is a fleet you host
 *   yourself;
 * - the QR code carries the pairing secret, and that route would have it decoded
 *   in another process before reaching us.
 *
 * What it costs: a camera permission the user can see, and the ~150 lines of
 * [ScanActivity] (CameraX + zxing, both Apache-2.0). What it buys beyond the
 * above: the scanner is an activity of THIS app, so the app is still the one in
 * front while it is up — Play Services' would have backgrounded us, and on this
 * OEM that means the network is cut mid-pairing (see [KeepAlive]).
 */
object ScanBridge {
    init {
        // The Tauri runtime's own `Rust` object already loads this library, but
        // only once its class is initialized. Loading it again is a no-op and
        // makes this seam independent of that ordering.
        System.loadLibrary("universallink_gui_mobile")
    }

    private const val TAG = "ULCore"

    /**
     * Passed to [ScanActivity]: the prefixes a code may carry, comma-separated
     * (`UL1:` pairs through a server, `UL2:` over the local network — see
     * scan.rs). One extra, not two: the list is the Rust side's to compose, and
     * a third kind of code must not need a Kotlin change.
     */
    const val EXTRA_TAG = "pairing_tag"

    /**
     * The activity a scanner can be opened from. WEAK on purpose: this object
     * outlives every activity (it is loaded with the library), and holding the
     * last one strongly would keep a dead window's whole view tree alive.
     */
    @Volatile
    private var host: WeakReference<Activity> = WeakReference(null)

    /**
     * Hands this class to the Rust side, along with the one fact about this
     * device it cannot read for itself. Called from MainActivity.onCreate.
     */
    fun attach(activity: Activity) {
        host = WeakReference(activity)
        // FEATURE_CAMERA_ANY rather than the rear camera: a tablet or a TV may
        // have only a front one, which reads a code just as well.
        val hasCamera = activity.packageManager
            .hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY)
        try {
            nativeInit(hasCamera)
        } catch (t: Throwable) {
            // Without the seam the button is simply never offered: pairing still
            // works by pasting the code, rather than the app failing to start.
            Log.e(TAG, "the scanner seam could not be wired", t)
        }
    }

    /** Rust captures this class, and whether there is a camera. See scan.rs. */
    @JvmStatic external fun nativeInit(hasCamera: Boolean)

    /** Reports one scan, as JSON: `{"code":…}` or `{"reason":…}`. See scan.rs. */
    @JvmStatic external fun onScanResult(json: String)

    /**
     * Opens the scanner — **called from Rust, on any thread**. `false` means it
     * never started and no result is coming: the window is gone (the user closed
     * the app while a command was in flight), or the JVM is shutting down.
     *
     * `tags` is what a code may start with to be one of ours, comma-separated.
     * It comes from the Core's own constants rather than being written down
     * here, so the format has one home.
     */
    @JvmStatic
    fun startScan(tags: String): Boolean {
        val activity = host.get() ?: return false
        val intent = Intent(activity, ScanActivity::class.java).putExtra(EXTRA_TAG, tags)
        // Starting an activity is main-thread work, and Rust calls from a worker.
        return Handler(Looper.getMainLooper()).post {
            try {
                activity.startActivity(intent)
            } catch (t: Throwable) {
                Log.e(TAG, "the scanner could not be opened", t)
                report(failure("failed"))
            }
        }
    }

    /**
     * Hands an outcome to the Rust side. Swallows nothing except its own failure
     * to cross: a scan that cannot be reported is one the frontend waits out (see
     * scan.rs `BACKSTOP`), and there is nothing better to do about it here.
     */
    fun report(json: String) {
        try {
            onScanResult(json)
        } catch (t: Throwable) {
            Log.e(TAG, "could not report a scan", t)
        }
    }

    /** `{"code":…}`. Built with JSONObject, which escapes: the text is a payload
     *  read off a stranger's QR code, and quoting it by hand would let it write
     *  its own fields. */
    fun code(text: String): String = JSONObject().put("code", text).toString()

    /** `{"reason":…}` — one of the words scan.rs knows. */
    fun failure(reason: String): String = JSONObject().put("reason", reason).toString()
}
