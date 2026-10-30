package com.chatter.client

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.LifecycleRegistry

/** Call media owns its lifecycle independently of Activity visibility. */
class CallMediaService : Service(), LifecycleOwner {
    private val registry = LifecycleRegistry(this)
    override val lifecycle: Lifecycle get() = registry
    private var cpuLock: PowerManager.WakeLock? = null
    private var camera = false
    private var stopping = false

    override fun onCreate() {
        super.onCreate()
        registry.currentState = Lifecycle.State.CREATED
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL, "Active calls", NotificationManager.IMPORTANCE_LOW)
        )
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (stopping) {
            finishCameraRequests(null)
            stopSelf()
            return START_NOT_STICKY
        }
        try {
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
            // Acquire only after promotion; a failed upgrade also releases the old lock.
            if (cpuLock == null) {
                cpuLock = getSystemService(PowerManager::class.java)
                    .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "chatter:call-media")
                    .also { it.acquire() }
            }
            registry.currentState = Lifecycle.State.STARTED
            currentService = this
            if (camera) finishCameraRequests(this)
        } catch (error: Exception) {
            // startForeground runs asynchronously, outside the enqueue caller's catch.
            Log.w(TAG, "Call media foreground promotion failed", error)
            stopping = true
            releaseMedia()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun releaseMedia() {
        if (currentService === this) currentService = null
        registry.currentState = Lifecycle.State.DESTROYED
        finishCameraRequests(null)
        cpuLock?.let { if (it.isHeld) it.release() }
        cpuLock = null
    }

    override fun onDestroy() {
        releaseMedia()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val TAG = "CallMediaService"
        private const val CHANNEL = "chatter_call_media"
        private const val ID = 0x434d
        // Called on the main looper, as are Service lifecycle callbacks.
        private var currentService: CallMediaService? = null
        private val cameraReady = mutableListOf<(LifecycleOwner?) -> Unit>()

        private fun finishCameraRequests(owner: LifecycleOwner?) {
            val callbacks = cameraReady.toList()
            cameraReady.clear()
            callbacks.forEach { ready ->
                runCatching { ready(owner) }
                    .onFailure { Log.w(TAG, "Call camera readiness callback failed", it) }
            }
        }

        fun start(context: Context) {
            // The active call UI can precede the first microphone grant. Permission
            // completion and Activity resume explicitly retry even with routing active.
            if (context.checkSelfPermission(Manifest.permission.RECORD_AUDIO) !=
                PackageManager.PERMISSION_GRANTED || currentService != null) return
            try {
                context.startForegroundService(Intent(context, CallMediaService::class.java))
            } catch (error: Exception) {
                Log.w(TAG, "Call media service start rejected", error)
            }
        }

        fun withCamera(context: Context, ready: (LifecycleOwner?) -> Unit) {
            if (context.checkSelfPermission(Manifest.permission.RECORD_AUDIO) !=
                PackageManager.PERMISSION_GRANTED ||
                context.checkSelfPermission(Manifest.permission.CAMERA) !=
                PackageManager.PERMISSION_GRANTED) {
                ready(null)
                return
            }
            // An already promoted camera needs no new background service admission.
            currentService?.takeIf { it.camera }?.let {
                ready(it)
                return
            }
            cameraReady.add(ready)
            try {
                context.startForegroundService(Intent(context, CallMediaService::class.java)
                    .putExtra("camera", true))
            } catch (error: Exception) {
                cameraReady.remove(ready)
                Log.w(TAG, "Call camera service start rejected", error)
                ready(null)
            }
        }

        fun stop(context: Context) {
            finishCameraRequests(null)
            context.stopService(Intent(context, CallMediaService::class.java))
        }
    }
}
