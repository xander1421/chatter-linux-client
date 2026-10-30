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
    private external fun nativeOnScreenStarted(token: Long, width: Int, height: Int)
    private external fun nativeOnScreenFrame(token: Long, bgra: ByteArray, width: Int, height: Int, timestampUs: Long)
    private external fun nativeOnScreenStopped(token: Long)

    private var token = 0L
    private var projection: MediaProjection? = null
    private var display: VirtualDisplay? = null
    private var reader: ImageReader? = null
    private var thread: HandlerThread? = null
    private var lastFrameNs = 0L
    private var stopping = false

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
            val scale = minOf(1.0, 1280.0 / maxOf(metrics.widthPixels, metrics.heightPixels))
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
                val image = try { source.acquireLatestImage() }
                    catch (_: IllegalStateException) { null }
                    ?: return@setOnImageAvailableListener
                try {
                    val now = SystemClock.elapsedRealtimeNanos()
                    if (now - lastFrameNs < 66_666_667L) return@setOnImageAvailableListener
                    lastFrameNs = now
                    val plane = image.planes[0]
                    val bytes = ByteArray(width * height * 4)
                    val buffer = plane.buffer
                    for (row in 0 until height) {
                        for (col in 0 until width) {
                            val at = row * plane.rowStride + col * plane.pixelStride
                            val out = (row * width + col) * 4
                            // ImageReader RGBA -> libyuv ARGB (BGRA byte order).
                            bytes[out] = buffer.get(at + 2)
                            bytes[out + 1] = buffer.get(at + 1)
                            bytes[out + 2] = buffer.get(at)
                            bytes[out + 3] = buffer.get(at + 3)
                        }
                    }
                    nativeOnScreenFrame(token, bytes, width, height, now / 1000)
                } catch (error: Exception) {
                    Log.e("ScreenShare", "Frame capture failed", error)
                    stopSelf()
                } finally {
                    try { image.close() } catch (_: IllegalStateException) { }
                }
            }, handler)
            // Install dimensions before ImageReader can deliver its first frame.
            nativeOnScreenStarted(token, width, height)
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
            runCatching { display?.release() }
            runCatching { reader?.close() }
            runCatching { projection?.stop() }
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

        fun start(context: Context, token: Long, resultCode: Int, data: Intent) {
            context.startForegroundService(Intent(context, ScreenShareService::class.java)
                .setAction(START).putExtra(TOKEN, token).putExtra(RESULT, resultCode)
                .putExtra(DATA, data))
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, ScreenShareService::class.java))
        }
    }
}
