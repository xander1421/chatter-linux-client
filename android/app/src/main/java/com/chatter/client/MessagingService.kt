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
import android.os.Handler
import android.os.Looper
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
    private var nativeStartupPending = false
    private external fun nativeStartHeadless(context: Context): Boolean

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
        if (stopRequested || !wasSignedIn(this)) {
            if (pendingStarts == 0) stopSelf(startId)
        } else if (coreReadyInProcess) {
            retainInboxExecution()
        } else if (!nativeStartupPending) {
            nativeStartupPending = true
            // Keep DB open and bounded network startup off Android's main
            // thread. JNI runs on this attached Java thread, not a Rust worker.
            Thread({
                val ready = try {
                    System.loadLibrary("chatter_ui")
                    nativeStartHeadless(applicationContext)
                } catch (error: Throwable) {
                    Log.e("MessagingService", "Headless core startup failed", error)
                    false
                }
                if (!ready) Log.w("MessagingService", "Native headless startup returned false")
                Handler(Looper.getMainLooper()).post {
                    nativeStartupPending = false
                    if (ready && !stopRequested && wasSignedIn(this)) {
                        coreReadyInProcess = true
                        retainInboxExecution()
                    } else if (pendingStarts == 0) {
                        stopSelf()
                    }
                }
            }, "ChatterHeadlessStart").start()
        } else {
            // A previous start already owns startup; its completion retains
            // execution or stops this service.
        }
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
            // Core calls this at the beginning of start(), before its inbox
            // poller exists. Only a successful nativeStartHeadless may mark
            // this process ready; it also waits for Activity startup if needed.
            if (!signedIn) coreReadyInProcess = false
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
