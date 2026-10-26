package com.bettersync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.util.Log

/**
 * Keeps the daemon running while the app is in the background. Android only
 * lets apps keep working in the background as a foreground service with a
 * visible notification.
 */
class SyncService : Service() {
    private var multicastLock: WifiManager.MulticastLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, getString(R.string.channel_name), NotificationManager.IMPORTANCE_LOW)
        )
        val notification = Notification.Builder(this, CHANNEL)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(getString(R.string.notification_text))
            .setSmallIcon(android.R.drawable.ic_popup_sync)
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }

        // Android drops multicast (mDNS) packets unless an app holds this.
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("bettersync-mdns").apply {
            setReferenceCounted(false)
            acquire()
        }

        val configDir = configDir(this)
        Thread {
            val result = Native.start(configDir)
            Log.i(TAG, "daemon: $result")
        }.start()
        return START_STICKY
    }

    override fun onDestroy() {
        Native.stop()
        multicastLock?.release()
        super.onDestroy()
    }

    companion object {
        private const val CHANNEL = "sync"
        private const val NOTIFICATION_ID = 1
        private const val TAG = "BetterSync"

        fun configDir(context: Context): String = context.filesDir.resolve("config").absolutePath
    }
}
