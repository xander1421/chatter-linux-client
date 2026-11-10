package com.chatter.client

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.AlarmManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log

/** Keeps the process owning core's inbox poller and ntfy listeners runnable.
 * No account, message, mailbox, or Tor logic is duplicated in Android glue.
 * specialUse describes continuous private messaging honestly: dataSync has a
 * six-hour background quota on Android 15+, and remoteMessaging is for relay
 * continuity between a user's devices, not ordinary incoming chat messages.
 */
class MessagingService : Service() {
    private var nativeStartupPending = false
    private external fun nativeStartHeadless(context: Context): Boolean

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
        } else if (!coreReadyInProcess && !nativeStartupPending) {
            nativeStartupPending = true
            // Keep DB open and bounded network startup off Android's main
            // thread. JNI runs on this attached Java thread, not a Rust worker.
            val startupLock = getSystemService(PowerManager::class.java)
                ?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "Chatter:MessagingStartup")
            startupLock?.acquire(60_000L)
            Thread({
                val ready = try {
                    System.loadLibrary("chatter_ui")
                    nativeStartHeadless(applicationContext)
                } catch (error: Throwable) {
                    Log.e("MessagingService", "Headless core startup failed", error)
                    false
                } finally {
                    if (startupLock?.isHeld == true) startupLock.release()
                }
                if (!ready) Log.w("MessagingService", "Native headless startup returned false")
                Handler(Looper.getMainLooper()).post {
                    nativeStartupPending = false
                    if (ready && !stopRequested && wasSignedIn(this)) {
                        coreReadyInProcess = true
                    } else if (pendingStarts == 0) {
                        stopSelf()
                    }
                }
            }, "ChatterHeadlessStart").start()
        } else {
            // Core is already running, or a previous start owns startup.
        }
        // Keep a Doze-tolerant heartbeat armed so a backgrounded or killed
        // process (whose in-process ntfy SSE listener died with it) is revived.
        if (!stopRequested && wasSignedIn(this)) scheduleKeepAlive(this)
        // Opt in to UnifiedPush if a distributor app is installed, so the core
        // can drop its own SSE socket. No distributor → no-op (own-SSE path).
        if (!stopRequested && wasSignedIn(this)) UnifiedPushReceiver.enableIfDistributorPresent(this)
        // START_STICKY: let the OS recreate the service (with a null intent,
        // handled above) after a low-memory kill, so push delivery resumes.
        return START_STICKY
    }

    // Swiped from recents: the hosting process is about to die, taking the SSE
    // listener with it. Re-arm via alarm so delivery comes back on its own
    // instead of staying dead until the user reopens the app.
    override fun onTaskRemoved(rootIntent: Intent?) {
        super.onTaskRemoved(rootIntent)
        if (!stopRequested && wasSignedIn(this)) scheduleRestartSoon(this)
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
            cancelKeepAlive(context)
            if (pendingStarts == 0) {
                context.stopService(Intent(context, MessagingService::class.java))
            }
        }

        private const val KEEPALIVE_REQUEST = 0x4349
        // Heartbeat cadence by network cost. The steady background drain is
        // cellular radio wakes (wifi is cheap), so on a metered link we wake
        // less often — 28 min also outlives cellular-carrier NAT timeouts, the
        // same reason FCM lengthens its heartbeat on mobile data. Unmetered
        // (wifi) keeps the former 15 min for faster socket revival.
        private const val KEEPALIVE_WIFI_MS = 15L * 60L * 1000L
        private const val KEEPALIVE_CELL_MS = 28L * 60L * 1000L

        private fun keepAlivePending(context: Context): PendingIntent =
            PendingIntent.getBroadcast(
                context,
                KEEPALIVE_REQUEST,
                Intent(context, MessagingKeepAliveReceiver::class.java),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )

        /** (Re)arm one Doze-tolerant heartbeat. Each firing re-arms the next via
         * MessagingKeepAliveReceiver, so the chain self-sustains while signed in. */
        fun scheduleKeepAlive(context: Context) {
            armAlarm(context, keepAliveIntervalMs(context))
        }

        /** Pick the heartbeat interval from the active link's metered flag.
         * Unknown network (no active link / no caps) falls back to the shorter
         * wifi interval, i.e. today's behaviour — never a longer gap than we'd
         * choose deliberately. A metered wifi hotspot counts as cellular here,
         * which is the battery-correct side to err on. */
        private fun keepAliveIntervalMs(context: Context): Long {
            val cm = context.getSystemService(ConnectivityManager::class.java)
                ?: return KEEPALIVE_WIFI_MS
            val caps = cm.activeNetwork?.let { cm.getNetworkCapabilities(it) }
                ?: return KEEPALIVE_WIFI_MS
            return if (caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)) {
                KEEPALIVE_WIFI_MS
            } else {
                KEEPALIVE_CELL_MS
            }
        }

        private fun scheduleRestartSoon(context: Context) {
            armAlarm(context, 2_000L)
        }

        private fun armAlarm(context: Context, delayMs: Long) {
            val am = context.getSystemService(AlarmManager::class.java) ?: return
            val at = SystemClock.elapsedRealtime() + delayMs
            val pending = keepAlivePending(context)
            // Exact-and-allow-while-idle fires in Doze AND grants the brief
            // background foreground-service-start allowlist the receiver needs to
            // revive the service. Fall back to the inexact variant when exact
            // alarms aren't permitted (API 31+ user revocation) rather than throw.
            if (Build.VERSION.SDK_INT >= 31 && !am.canScheduleExactAlarms()) {
                am.setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, at, pending)
            } else {
                am.setExactAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, at, pending)
            }
        }

        fun cancelKeepAlive(context: Context) {
            context.getSystemService(AlarmManager::class.java)
                ?.cancel(keepAlivePending(context))
        }
    }
}
