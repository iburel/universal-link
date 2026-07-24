package org.universallink.mobile

import android.Manifest
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
