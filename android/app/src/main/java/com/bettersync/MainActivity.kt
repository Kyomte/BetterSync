package com.bettersync

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.webkit.WebView
import android.widget.LinearLayout
import android.widget.TextView

/**
 * Shows the daemon's status page (pairing, folders, conflicts) — the same
 * page the desktop tray app opens in a browser.
 */
class MainActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT >= 33) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
        startForegroundService(Intent(this, SyncService::class.java))

        // Folders must be real paths the app can write; the app's external
        // files directory always works.
        val hint = TextView(this).apply {
            setPadding(32, 24, 32, 8)
            text = "Folders can live under:\n" + (getExternalFilesDir(null)?.absolutePath ?: filesDir.absolutePath)
            setTextIsSelectable(true)
        }
        val web = WebView(this).apply {
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(hint)
            addView(web, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))
        })

        // start() is idempotent: it returns the running daemon's page URL.
        Thread {
            val url = Native.start(SyncService.configDir(this))
            runOnUiThread {
                if (url.startsWith("http")) web.loadUrl(url) else hint.text = url
            }
        }.start()
    }
}
