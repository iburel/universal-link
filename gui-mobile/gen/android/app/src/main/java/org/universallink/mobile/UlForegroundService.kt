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
 * same process) keeps its outbound network while the app is backgrounded.
 *
 * Why this is needed: the OIDC login runs its token exchange while the user is
 * in the browser (app backgrounded), and P2P transfers run while the user is in
 * another app. Aggressive OEM power managers (OnePlus/Oppo were the ones we hit)
 * otherwise suspend a backgrounded app's outbound network within ~2s, which
 * surfaces as `getaddrinfo` failing with EAI_NODATA ("No address associated
 * with hostname"). A running foreground service prevents that suspension.
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

        val notification: Notification = Notification.Builder(this, channelId)
            .setContentTitle("UniversalLink")
            .setContentText("Keeping your connection alive")
            .setSmallIcon(applicationInfo.icon)
            .setOngoing(true)
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
        return START_STICKY
    }

    override fun onTaskRemoved(rootIntent: Intent?) {
        // The user swiped the app away: stop keeping the process alive.
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
        super.onTaskRemoved(rootIntent)
    }

    companion object {
        private const val NOTIFICATION_ID = 1

        fun start(context: Context) {
            val intent = Intent(context, UlForegroundService::class.java)
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
