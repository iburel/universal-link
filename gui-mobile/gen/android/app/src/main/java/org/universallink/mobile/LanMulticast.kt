package org.universallink.mobile

import android.app.Activity
import android.app.Application
import android.content.Context
import android.net.wifi.WifiManager
import android.os.Bundle
import android.util.Log

/**
 * The other half of the Core's LAN discovery: Android's Wi-Fi driver drops
 * incoming multicast unless an app holds a [WifiManager.MulticastLock], so
 * without this object the embedded Core announces itself over mDNS but never
 * hears an answer — every desktop would show the phone "on this network" while
 * the phone sees no one.
 *
 * The filter exists to save battery (each multicast frame on the network wakes
 * the Wi-Fi chip), so the lock is not simply held for the process's lifetime:
 * it follows the same rule as [UlForegroundService] — held while the app has a
 * window in front OR the Core reports work in flight, released the moment
 * neither is true. A cached background process thus goes deaf, which takes
 * nothing it could use: hearing a peer only ever serves a screen the user is
 * looking at or a transfer the service is protecting, and this OEM suspends a
 * backgrounded app's network within seconds anyway (see [KeepAlive]).
 *
 * Visibility is counted over ALL of this app's activities (a lifecycle
 * callback on the Application), not read off MainActivity alone: ScanActivity
 * in front stops MainActivity, and the pairing screen is no reason to fall off
 * the network. Android starts the incoming activity before stopping the
 * outgoing one, so the count never dips to zero across a transition.
 */
object LanMulticast {
    private const val TAG = "ULCore"

    /** Created on first need, then reused. Null on a device with no Wi-Fi. */
    private var lock: WifiManager.MulticastLock? = null

    /** Started (visible) activities of ours. */
    private var windows = 0

    /** Work in flight, as [KeepAlive] last reported it. */
    private var working = false

    private var attached = false

    /**
     * Wires the visibility tracking up. Called from MainActivity.onCreate —
     * before its own onStart, so the first window is counted — and idempotent,
     * because a process restore recreates the activity and calls it again.
     */
    @Synchronized
    fun attach(activity: Activity) {
        if (attached) return
        attached = true
        activity.application.registerActivityLifecycleCallbacks(object :
            Application.ActivityLifecycleCallbacks {
            override fun onActivityStarted(a: Activity) = window(a, +1)
            override fun onActivityStopped(a: Activity) = window(a, -1)
            override fun onActivityCreated(a: Activity, state: Bundle?) {}
            override fun onActivityResumed(a: Activity) {}
            override fun onActivityPaused(a: Activity) {}
            override fun onActivitySaveInstanceState(a: Activity, state: Bundle) {}
            override fun onActivityDestroyed(a: Activity) {}
        })
    }

    /** The work in flight changed — from [KeepAlive.setWork], any thread. */
    @Synchronized
    fun work(context: Context, working: Boolean) {
        this.working = working
        refresh(context)
    }

    @Synchronized
    private fun window(activity: Activity, delta: Int) {
        windows += delta
        refresh(activity)
    }

    private fun refresh(context: Context) {
        val wanted = windows > 0 || working
        try {
            val lock = this.lock ?: run {
                // No reason to hold it yet: leave even the lock uncreated.
                if (!wanted) return
                val wifi = context.applicationContext
                    .getSystemService(Context.WIFI_SERVICE) as WifiManager
                wifi.createMulticastLock("universallink-lan").also {
                    // Held or not, nothing in between: the two callers above
                    // both converge on `wanted` rather than pairing their own
                    // acquire/release.
                    it.setReferenceCounted(false)
                    this.lock = it
                }
            }
            if (wanted && !lock.isHeld) {
                lock.acquire()
                Log.i(TAG, "multicast lock acquired: the LAN is audible")
            } else if (!wanted && lock.isHeld) {
                lock.release()
                Log.i(TAG, "multicast lock released")
            }
        } catch (t: Throwable) {
            // A device with no Wi-Fi service, or a lock the system refuses:
            // the Core just stays deaf on the LAN — everything the server and
            // the relay carry still works, so this must not take the app down.
            Log.e(TAG, "could not follow the multicast lock state", t)
        }
    }
}
