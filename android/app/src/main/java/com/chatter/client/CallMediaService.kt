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
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.LifecycleRegistry

/** Call media owns its lifecycle independently of Activity visibility. */
class CallMediaService : Service(), LifecycleOwner {
    private val registry = LifecycleRegistry(this)
    override val lifecycle: Lifecycle get() = registry
    private var cpuLock: PowerManager.WakeLock? = null
    private var camera = false

    override fun onCreate() {
        super.onCreate()
        registry.currentState = Lifecycle.State.CREATED
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL, "Active calls", NotificationManager.IMPORTANCE_LOW)
        )
        cpuLock = getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "chatter:call-media")
            .also { it.acquire() }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        camera = camera || intent?.getBooleanExtra("camera", false) == true
        val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val notification = Notification.Builder(this, CHANNEL)
            .setContentTitle("Chatter call in progress")
            .setSmallIcon(android.R.drawable.sym_call_incoming)
            .setContentIntent(open).setOngoing(true).build()
        if (Build.VERSION.SDK_INT >= 30) {
            val types = ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE or
                (if (camera) ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA else 0)
            startForeground(ID, notification, types)
        } else {
            startForeground(ID, notification)
        }
        registry.currentState = Lifecycle.State.STARTED
        if (camera) {
            val callbacks = cameraReady.toList()
            cameraReady.clear()
            callbacks.forEach { it(this) }
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        registry.currentState = Lifecycle.State.DESTROYED
        cameraReady.clear()
        cpuLock?.let { if (it.isHeld) it.release() }
        cpuLock = null
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val CHANNEL = "chatter_call_media"
        private const val ID = 0x434d
        // Called on the main looper, as are Service lifecycle callbacks.
        private val cameraReady = mutableListOf<(LifecycleOwner) -> Unit>()

        fun start(context: Context) {
            context.startForegroundService(Intent(context, CallMediaService::class.java))
        }

        fun withCamera(context: Context, ready: (LifecycleOwner) -> Unit) {
            cameraReady.add(ready)
            try {
                context.startForegroundService(Intent(context, CallMediaService::class.java)
                    .putExtra("camera", true))
            } catch (error: Exception) {
                cameraReady.remove(ready)
                throw error
            }
        }

        fun stop(context: Context) {
            cameraReady.clear()
            context.stopService(Intent(context, CallMediaService::class.java))
        }
    }
}
