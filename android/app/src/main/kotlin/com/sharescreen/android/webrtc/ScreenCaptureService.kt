package com.sharescreen.android.webrtc

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat

/**
 * Foreground service required for MediaProjection on Android 14+.
 * Keeps the process alive with a persistent notification while screen
 * sharing is active. The actual capture is handled by [WebRtcManager].
 */
class ScreenCaptureService : Service() {

    companion object {
        private const val CHANNEL_ID = "screen_capture"
        private const val NOTIFICATION_ID = 1

        /** Set while waiting for [startForeground]; Android 14 rejects MediaProjection until then. */
        private var pendingOnStarted: (() -> Unit)? = null

        /**
         * Starts the foreground service and runs [onStarted] after `startForeground`
         * has run. Android 14+ throws SecurityException if MediaProjection starts
         * before a mediaProjection foreground service is actually in the foreground.
         */
        fun start(context: Context, onStarted: () -> Unit = {}) {
            pendingOnStarted = onStarted
            val intent = Intent(context, ScreenCaptureService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        fun stop(context: Context) {
            pendingOnStarted = null
            context.stopService(Intent(context, ScreenCaptureService::class.java))
        }
    }

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val notification = NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(com.sharescreen.android.R.string.screen_capture_notification_title))
            .setContentText(getString(com.sharescreen.android.R.string.screen_capture_notification_text))
            .setSmallIcon(android.R.drawable.ic_media_play)
            .setOngoing(true)
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }

        pendingOnStarted?.let { onStarted ->
            pendingOnStarted = null
            onStarted()
        }

        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                getString(com.sharescreen.android.R.string.screen_capture_channel_name),
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = getString(com.sharescreen.android.R.string.screen_capture_channel_description)
            }
            getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        }
    }
}
