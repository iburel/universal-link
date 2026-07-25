package org.universallink.mobile

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder

/**
 * A minimal foreground service. It does no work of its own: it exists only to
 * elevate the app's PROCESS priority so the embedded Core (which runs in this
 * same process) keeps its outbound network while the app is not in front.
 *
 * Why this is needed: the OIDC login runs its token exchange while the user is in
 * the browser (app backgrounded), and a P2P transfer runs while the user is in
 * another app. Aggressive OEM power managers (OnePlus/Oppo were the ones we hit)
 * otherwise suspend a backgrounded app's outbound network within ~2s, which
 * surfaces as `getaddrinfo` failing with EAI_NODATA ("No address associated with
 * hostname"). A running foreground service prevents that suspension — proven on
 * the device, where a 300 MiB send finished with the app behind HOME and an
 * `am kill` aimed at it.
 *
 * It runs while there is WORK, not while there is an activity: [KeepAlive] starts
 * and stops it from what the Core reports. So the shade stays clean while the app
 * is merely open, and work outlives the app leaving the foreground. A swipe out of
 * the Recents list is a different matter on this OEM — see [KeepAlive].
 */
class UlForegroundService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val channelId = "universallink.active"
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                channelId,
                "UniversalLink activity",
                NotificationManager.IMPORTANCE_LOW,
            )
            channel.setShowBadge(false)
            (getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
                .createNotificationChannel(channel)
        }

        // An intent-less start (Android re-delivering one): KeepAlive's last word
        // is the only truth we have left.
        val kind = intent?.getIntExtra(EXTRA_WORK, KeepAlive.work) ?: KeepAlive.work
        val notification: Notification = Notification.Builder(this, channelId)
            .setContentTitle("UniversalLink")
            .setContentText(text(kind))
            .setSmallIcon(applicationInfo.icon)
            .setOngoing(true)
            .build()

        // ALWAYS, even to leave straight away: `startForegroundService` is a
        // promise to call this, and the platform kills the app if a start it
        // delivered goes unanswered.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }

        // The work ended while this start was in flight (a transfer that failed
        // at once, a login refused on the spot): the promise above is kept, and
        // there is nothing left to hold the process up for.
        if (kind == KeepAlive.NONE) {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
        // NOT sticky: this service protects a process that is gone by the time
        // Android would restart it — a lone service would then keep an empty app
        // alive, notification included, protecting nothing.
        return START_NOT_STICKY
    }

    /** What the ongoing notification says we are holding the process up for. */
    private fun text(kind: Int): String = when (kind) {
        KeepAlive.TRANSFER -> "Transferring files"
        KeepAlive.SHARE -> "Sharing with your devices"
        KeepAlive.AUTH -> "Waiting for your browser"
        else -> "Keeping your connection alive"
    }

    override fun onTaskRemoved(rootIntent: Intent?) {
        // The user swiped the app away. Work in flight outlives that gesture on
        // purpose: a transfer they started is not cancelled by tidying up the
        // Recents list, and it is the work ending that stops this service.
        //
        // On this OEM the point is moot — OxygenOS kills the whole process on the
        // swipe, service or no service (see [KeepAlive]) — so this is what a
        // platform following the AOSP contract gets, at no cost here.
        if (KeepAlive.work == KeepAlive.NONE) {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
        super.onTaskRemoved(rootIntent)
    }

    companion object {
        private const val NOTIFICATION_ID = 1
        private const val EXTRA_WORK = "work"

        fun start(context: Context, kind: Int) {
            val intent = Intent(context, UlForegroundService::class.java)
                .putExtra(EXTRA_WORK, kind)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, UlForegroundService::class.java))
        }
    }
}
