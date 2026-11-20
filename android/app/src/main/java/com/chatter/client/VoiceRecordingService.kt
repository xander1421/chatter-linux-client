package com.chatter.client

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager

/** Owns only an explicitly started voice-note capture, independent of window focus. */
class VoiceRecordingService : Service() {
    private var token = 0L
    private var cpuLock: PowerManager.WakeLock? = null

    override fun onCreate() {
        super.onCreate()
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL, getString(R.string.voice_recording_channel), NotificationManager.IMPORTANCE_LOW)
        )
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val incoming = intent?.getLongExtra("token", 0L) ?: 0L
        synchronized(Companion) {
            if (incoming == 0L || incoming != requested) {
                stopSelf(startId)
                return START_NOT_STICKY
            }
            token = incoming
            try {
                val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java),
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
                val notification = Notification.Builder(this, CHANNEL)
                    .setSmallIcon(android.R.drawable.ic_btn_speak_now)
                    .setContentTitle(getString(R.string.voice_recording_title))
                    .setContentIntent(open).setOngoing(true).setOnlyAlertOnce(true).build()
                if (Build.VERSION.SDK_INT >= 30) {
                    startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
                } else {
                    startForeground(ID, notification)
                }
                cpuLock?.let { if (it.isHeld) it.release() }
                cpuLock = getSystemService(PowerManager::class.java)
                    .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "chatter:voice-recording")
                    .also { it.acquire(310_000L) } // Core caps capture at five minutes.
                ready = token
            } catch (_: Exception) {
                requested = 0L
                ready = 0L
                stopSelf()
            }
        }
        return START_NOT_STICKY // Never resume microphone capture after process death.
    }

    override fun onDestroy() {
        synchronized(Companion) {
            if (requested == token) { requested = 0L; ready = 0L }
        }
        cpuLock?.let { if (it.isHeld) it.release() }
        cpuLock = null
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val CHANNEL = "chatter_voice_recording"
        private const val ID = 0x5652
        private var next = 0L
        private var requested = 0L
        private var ready = 0L

        @Synchronized fun start(context: Context): Long {
            if (requested != 0L) return 0L
            val token = ++next
            requested = token
            ready = 0L
            try {
                context.startForegroundService(Intent(context, VoiceRecordingService::class.java).putExtra("token", token))
            } catch (_: Exception) {
                requested = 0L
                return 0L
            }
            return token
        }

        @Synchronized fun status(token: Long): Int =
            if (token == 0L || requested != token) -1 else if (ready == token) 1 else 0

        @Synchronized fun stop(context: Context, token: Long) {
            // A cancelled permission/startup task must never stop a newer recording.
            if (token == 0L || requested != token) return
            requested = 0L
            ready = 0L
            context.stopService(Intent(context, VoiceRecordingService::class.java))
        }
    }
}
