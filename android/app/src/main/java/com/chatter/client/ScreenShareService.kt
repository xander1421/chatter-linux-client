package com.chatter.client

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.view.WindowManager
import android.net.ConnectivityManager
import java.nio.ByteBuffer
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.SystemClock
import android.util.DisplayMetrics
import android.util.Log

/** Owns the projection and its typed foreground-service lifetime. */
class ScreenShareService : Service() {
    private external fun nativeOnScreenStarted(token: Long, width: Int, height: Int, metered: Boolean)
    private external fun nativeOnScreenFrame(token: Long, rgba: ByteBuffer, rowStride: Int, pixelStride: Int, width: Int, height: Int, timestampUs: Long, metered: Boolean)
    private external fun nativeOnScreenStopped(token: Long)

    private val frameLock = Any()
    private var token = 0L
    private var projection: MediaProjection? = null
    private var display: VirtualDisplay? = null
    private var reader: ImageReader? = null
    private var thread: HandlerThread? = null
    private var nextFrameNs = 0L
    private var stopping = false
    private var targetFps = 60
    private var metered = true
    private var lastNetworkCheckNs = 0L

    private fun updateNetwork(now: Long) {
        if (lastNetworkCheckNs == 0L || now - lastNetworkCheckNs >= 2_000_000_000L) {
            metered = getSystemService(ConnectivityManager::class.java).isActiveNetworkMetered
            lastNetworkCheckNs = now
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL, "Screen sharing", NotificationManager.IMPORTANCE_LOW)
        )
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val requestedToken = intent?.takeIf { it.action == START }?.getLongExtra(TOKEN, 0L) ?: 0L
        if (token != 0L) {
            // A second start must not stop the projection already owned by this
            // service. Tell its caller that this separate request did not start.
            if (requestedToken != 0L && requestedToken != token) nativeOnScreenStopped(requestedToken)
            return START_NOT_STICKY
        }
        if (requestedToken == 0L) {
            stopSelf(startId)
            return START_NOT_STICKY
        }
        val start = intent ?: return START_NOT_STICKY
        token = requestedToken
        targetFps = start.getIntExtra(FPS, 60).coerceIn(1, 60)
        updateNetwork(SystemClock.elapsedRealtimeNanos())
        try {
            val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
            val notification = Notification.Builder(this, CHANNEL)
                .setContentTitle("Chatter is sharing your screen")
                .setSmallIcon(android.R.drawable.ic_menu_view)
                .setContentIntent(open).setOngoing(true).build()
            // Android 14+ rejects getMediaProjection until this promotion succeeds.
            if (Build.VERSION.SDK_INT >= 34) {
                startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
            } else {
                startForeground(ID, notification)
            }
            val manager = getSystemService(MediaProjectionManager::class.java)
            @Suppress("DEPRECATION")
            val consent = if (Build.VERSION.SDK_INT >= 33)
                start.getParcelableExtra(DATA, Intent::class.java)
                else start.getParcelableExtra(DATA)
            val granted = consent ?: error("Missing projection consent")
            val projection = manager.getMediaProjection(start.getIntExtra(RESULT, 0), granted)
                ?: error("MediaProjection unavailable")
            this.projection = projection
            val metrics = DisplayMetrics()
            getSystemService(WindowManager::class.java).defaultDisplay.getRealMetrics(metrics)
            val scale = minOf(1.0, 1920.0 / maxOf(metrics.widthPixels, metrics.heightPixels))
            val width = ((metrics.widthPixels * scale).toInt() and -2).coerceAtLeast(2)
            val height = ((metrics.heightPixels * scale).toInt() and -2).coerceAtLeast(2)
            val worker = HandlerThread("ChatterScreenCapture").also { it.start() }
            thread = worker
            val handler = Handler(worker.looper)
            projection.registerCallback(object : MediaProjection.Callback() {
                override fun onStop() { stopSelf() }
            }, handler)
            val reader = ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 2)
            this.reader = reader
            reader.setOnImageAvailableListener({ source ->
                synchronized(frameLock) {
                    val image = try { source.acquireLatestImage() }
                        catch (_: IllegalStateException) { null }
                        ?: return@setOnImageAvailableListener
                    try {
                        val now = SystemClock.elapsedRealtimeNanos()
                        val periodNs = 1_000_000_000L / targetFps
                        if (now < nextFrameNs) return@setOnImageAvailableListener
                        // Half a period of scheduling credit tolerates vsync jitter
                        // without accumulating a burst after capture stalls.
                        nextFrameNs = maxOf(nextFrameNs + periodNs, now + periodNs / 2)
                        val plane = image.planes[0]
                        updateNetwork(now)
                        // Synchronous JNI borrows the plane only until this returns.
                        // acquireLatestImage + two slots keeps stale frames out of the pipeline.
                        nativeOnScreenFrame(token, plane.buffer, plane.rowStride, plane.pixelStride,
                            width, height, now / 1000, metered)
                    } catch (error: Exception) {
                        Log.e("ScreenShare", "Frame capture failed", error)
                        stopSelf()
                    } finally {
                        try { image.close() } catch (_: IllegalStateException) { }
                    }
                }
            }, handler)
            // Install dimensions before ImageReader can deliver its first frame.
            nativeOnScreenStarted(token, width, height, metered)
            display = projection.createVirtualDisplay("Chatter screen", width, height,
                metrics.densityDpi, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                reader.surface, null, handler)
        } catch (error: Exception) {
            Log.e("ScreenShare", "Projection start failed", error)
            stopSelf()
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        if (!stopping) {
            stopping = true
            val ended = token
            token = 0L
            runCatching { reader?.setOnImageAvailableListener(null, null) }
            // ImageReader.close invalidates plane memory. Wait for the one
            // synchronous JNI borrower before releasing its backing storage.
            synchronized(frameLock) {
                runCatching { display?.release() }
                runCatching { reader?.close() }
                runCatching { projection?.stop() }
            }
            thread?.quitSafely()
            if (ended != 0L) nativeOnScreenStopped(ended)
        }
        super.onDestroy()
    }

    companion object {
        private const val CHANNEL = "chatter_screen_share"
        private const val ID = 0x4353
        private const val START = "com.chatter.client.START_SCREEN_SHARE"
        private const val TOKEN = "token"
        private const val RESULT = "result"
        private const val DATA = "data"
        private const val FPS = "fps"

        fun start(context: Context, token: Long, resultCode: Int, data: Intent, fps: Int = 60) {
            context.startForegroundService(Intent(context, ScreenShareService::class.java)
                .setAction(START).putExtra(TOKEN, token).putExtra(RESULT, resultCode)
                .putExtra(DATA, data).putExtra(FPS, fps))
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, ScreenShareService::class.java))
        }
    }
}
