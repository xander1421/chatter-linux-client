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
import android.annotation.SuppressLint
import android.util.Log

/** Keeps the process owning core's inbox poller and ntfy listeners runnable.
 * No account, message, mailbox, or Tor logic is duplicated in Android glue.
 * specialUse describes continuous private messaging honestly: dataSync has a
 * six-hour background quota on Android 15+, and remoteMessaging is for relay
 * continuity between a user's devices, not ordinary incoming chat messages.
 */
class MessagingService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null

    // The service lifetime is the bound: release onDestroy, and never restart
    // after process death. Doze can still suspend network/ignore wake locks.
    @SuppressLint("WakelockTimeout")
    private fun retainInboxExecution() {
        if (wakeLock?.isHeld == true) return
        wakeLock = getSystemService(PowerManager::class.java)
            ?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "Chatter:Messaging")
            ?.apply { setReferenceCounted(false); acquire() }
    }

    override fun onDestroy() {
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
        super.onDestroy()
    }

    override fun onCreate() {
        super.onCreate()
        getSystemService(NotificationManager::class.java)?.createNotificationChannel(
            NotificationChannel(CHANNEL, "Connection", NotificationManager.IMPORTANCE_LOW).apply {
                setShowBadge(false)
            }
        )
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java).apply {
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        },
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val notification = Notification.Builder(this, CHANNEL)
            .setContentTitle("Chatter")
            .setContentText("Chatter is connected")
            .setSmallIcon(android.R.drawable.stat_notify_chat)
            .setContentIntent(open).setOngoing(true).setShowWhen(false).build()
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(ID, notification)
        }
        // Every startForegroundService request must be promoted, even if the
        // activity returned to the foreground before this command ran.
        if (pendingStarts > 0) pendingStarts--
        if (stopRequested || !coreReadyInProcess) {
            // A boot/update broadcast can create this service in a fresh
            // process. Until Rust has a headless startup entry, no inbox
            // poller exists there; do not hold a wake lock or claim connected.
            if (pendingStarts == 0) stopSelf(startId)
        } else {
            retainInboxExecution()
        }
        // Rust state cannot be reconstructed by a service-only process restart.
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val CHANNEL = "chatter_connection"
        private const val ID = 0x4348
        private const val PREFS = "messaging_lifecycle"
        private const val SIGNED_IN = "signed_in"
        // Activity lifecycle callbacks and service callbacks run on the main thread.
        private var pendingStarts = 0
        private var stopRequested = false
        private var coreReadyInProcess = false
        fun wasSignedIn(context: Context): Boolean =
            context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                .getBoolean(SIGNED_IN, false)

        fun setSignedIn(context: Context, signedIn: Boolean) {
            // commit makes the marker durable before an update kills this process.
            context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                .edit().putBoolean(SIGNED_IN, signedIn).commit()
            coreReadyInProcess = signedIn
        }
        fun start(context: Context) {
            stopRequested = false
            pendingStarts++
            try {
                // POST_NOTIFICATIONS denial does not prohibit an FGS: Android
                // shows it in Active apps instead of the notification drawer.
                context.startForegroundService(Intent(context, MessagingService::class.java))
            } catch (_: Exception) {
                pendingStarts--
                Log.w("MessagingService", "Foreground messaging start refused")
            }
        }
        fun stop(context: Context) {
            stopRequested = true
            if (pendingStarts == 0) {
                context.stopService(Intent(context, MessagingService::class.java))
            }
        }
    }
}
