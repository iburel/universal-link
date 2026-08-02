package org.universallink.mobile

import android.content.Context
import android.util.Log

/**
 * The Kotlin end of the process's keep-alive: the Rust side says what work is in
 * flight, this turns it into a foreground service — running while there is work,
 * gone as soon as there is none.
 *
 * This is the only Rust→Kotlin seam in the app (ShareBridge goes the other way),
 * and it exists because the two halves of the decision live on opposite sides:
 * only the Core knows a transfer is running, and only Kotlin can start a service.
 * [nativeInit] is what hands the Rust side this class to call back into
 * (`keepalive.rs`), so it must run before the embedded Core can report anything —
 * see MainActivity, which calls [attach] first thing.
 *
 * Why bother at all: a backgrounded Android app is promised nothing, and this OEM
 * (OnePlus) cuts a backgrounded app's outbound network within seconds — which is
 * exactly what a login's browser round-trip and a running transfer need. Tying
 * the service to the WORK rather than to the activity is what lets a transfer
 * outlive the app leaving the foreground, and what keeps a permanent notification
 * off the shade the rest of the time.
 *
 * What it does NOT buy, measured on the device: surviving a swipe out of the
 * Recents list. OxygenOS answers that gesture by tearing the service down and
 * killing the process in the same millisecond
 * (`UserAwareMgr: process killed … flags='bg'`), foreground service or not — a
 * 300 MiB transfer died 31 s in. The guard in [UlForegroundService.onTaskRemoved]
 * is the right thing on a platform that honours the AOSP contract, and costs
 * nothing here; on this one, HOME is what a transfer survives, not a swipe.
 */
object KeepAlive {
    /**
     * Kinds of work, mirrored by `keepalive.rs`. The values are ordered by
     * priority: when several overlap, the notification names the greatest, since
     * it can only say one thing. AUTH is a round-trip through the browser (a
     * login, or a revocation the server made us re-authorize).
     */
    const val NONE = 0
    const val AUTH = 1
    const val SHARE = 2
    const val TRANSFER = 3

    private const val TAG = "ULCore"

    /** The application context, from [attach]. Null until then. */
    @Volatile
    private var context: Context? = null

    /**
     * The work last reported by the Rust side. Read by the service when the task
     * is removed: with work in flight, a swipe must not take the transfer down.
     */
    @Volatile
    var work: Int = NONE
        private set

    init {
        // The Tauri runtime's `Rust` object loads this library too, but only when
        // its class is initialized; loading it again is a no-op and keeps this
        // seam independent of that ordering.
        System.loadLibrary("universallink_gui_mobile")
    }

    /** Hands this class to the Rust side. See `keepalive.rs`. */
    @JvmStatic external fun nativeInit()

    /** Wires the seam up, before the Core boots (MainActivity.onCreate). */
    fun attach(context: Context) {
        this.context = context.applicationContext
        try {
            nativeInit()
        } catch (t: Throwable) {
            // Without the seam the service is simply never asked for: work runs
            // unprotected, rather than the app failing to start.
            Log.e(TAG, "the keep-alive seam could not be wired", t)
        }
    }

    /**
     * The work in flight changed — **called from Rust, on any thread**. Idempotent
     * on purpose: the Rust side only calls when its answer changes, but a repeat
     * would just refresh the notification.
     */
    @JvmStatic
    fun setWork(kind: Int) {
        work = kind
        val context = this.context ?: return
        // Work in flight is also one of the two reasons to hear the LAN: a
        // transfer running behind HOME may have to re-resolve its peer, and
        // with every window stopped nothing else holds the multicast lock.
        LanMulticast.work(context, kind != NONE)
        try {
            if (kind == NONE) {
                UlForegroundService.stop(context)
            } else {
                UlForegroundService.start(context, kind)
            }
        } catch (t: Throwable) {
            // Android 12+ refuses a foreground service started from the
            // background. All the work the user STARTS is started from the app in
            // the foreground; what is left is work that begins without any UI (an
            // incoming transfer), which then runs unprotected instead of crashing
            // the process on a ForegroundServiceStartNotAllowedException.
            Log.e(TAG, "could not follow the work state (kind=$kind)", t)
        }
    }
}
