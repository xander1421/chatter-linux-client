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
import android.util.Log

/**
 * Keeps the mesh radio alive while the app is in the background.
 *
 * Without a foreground service Android stops the radio the moment the app stops
 * being visible, and a mesh node that only works while you are staring at it is
 * not a mesh node. The service type is `connectedDevice`, which is the category
 * the platform expects for a Bluetooth peer session and the one declared in the
 * manifest — an FGS whose runtime type and manifest type disagree is refused
 * outright on Android 14+.
 *
 * The service holds no radio state of its own. [MeshBleBridge] owns the
 * advertiser, the GATT server and every link; this exists solely to buy them
 * background execution, and is started and stopped by the bridge.
 */
class MeshService : Service() {

    override fun onCreate() {
        super.onCreate()
        // A service context is a perfectly good application context, and it is
        // available here whether the bridge got one any other way or not.
        MeshBleBridge.attach(applicationContext)
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        try {
            val notification = buildNotification()
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                startForeground(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
                )
            } else {
                startForeground(NOTIFICATION_ID, notification)
            }
        } catch (e: Exception) {
            // Android 12+ refuses foreground starts from the background. The radio
            // itself is unaffected and keeps running while the app is visible, so
            // this is a downgrade rather than a failure — but stop the service, or
            // the system kills it in five seconds anyway for not going foreground.
            Log.e(TAG, "foreground start refused; mesh will not survive backgrounding", e)
            stopSelf()
        }
        // Deliberately not sticky: all the mesh state lives in the Rust side of
        // this process, so a service the system revives on its own would show an
        // "active" notification over a radio that no longer exists.
        return START_NOT_STICKY
    }

    /** Nothing to bind to — the bridge is reached over JNI, not over IPC. */
    override fun onBind(intent: Intent?): IBinder? = null

    private fun createChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            CHANNEL_NAME,
            // Low: the mesh is ambient. It must be visible, never audible.
            NotificationManager.IMPORTANCE_LOW,
        )
        channel.setShowBadge(false)
        channel.description = "Shown while this device is relaying mesh traffic."
        val manager = getSystemService(NotificationManager::class.java)
        manager?.createNotificationChannel(channel)
    }

    private fun buildNotification(): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).apply {
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
            },
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Mesh active")
            .setContentText("Relaying nearby messages over Bluetooth")
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setContentIntent(open)
            .setOngoing(true)
            .setShowWhen(false)
            .build()
    }

    companion object {
        private const val TAG = "MeshService"

        /** Distinct from the channels Rust creates (`chatter_messages`, `chatter_calls`). */
        private const val CHANNEL_ID = "chatter_mesh"
        private const val CHANNEL_NAME = "Mesh"

        /** Fixed: there is exactly one mesh notification for the life of the process. */
        private const val NOTIFICATION_ID = 0x4D45 // 'ME'

        /**
         * Bring the service up. Called by [MeshBleBridge.start].
         *
         * Failure is logged and swallowed: on Android 12+ this throws if the app is
         * in the background, and the radio still works while the app is visible.
         */
        fun start(context: Context) {
            try {
                context.startForegroundService(Intent(context, MeshService::class.java))
            } catch (e: Exception) {
                Log.w(TAG, "could not start the mesh foreground service", e)
            }
        }

        /** Take the service down. Called by [MeshBleBridge.stop]. */
        fun stop(context: Context) {
            try {
                context.stopService(Intent(context, MeshService::class.java))
            } catch (e: Exception) {
                Log.w(TAG, "could not stop the mesh foreground service", e)
            }
        }
    }
}
