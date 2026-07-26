package com.chatter.client

import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.util.Log
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.core.content.ContextCompat
import com.google.androidgamesdk.GameActivity
import java.io.File
import java.util.concurrent.Executors

/**
 * GameActivity host for the Slint/Rust client.
 *
 * The native side (`android_main`, GameActivity glue) lives in
 * `libchatter_ui.so`, loaded via the `android.app.lib_name` manifest meta-data.
 * This subclass hosts the OS-facing bridges that Slint/Rust can't do directly,
 * surfacing results back to Rust over JNI. The Rust side reaches *into* Android
 * (e.g. notifications) via `ndk-context`; Android reaches *back* via the
 * `native…` methods declared here.
 */
class MainActivity : GameActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Hand the Activity to Rust. ndk-context only exposes the Application,
        // which lacks our Activity methods (launchFilePicker, requestPermissions),
        // so Rust holds a GlobalRef to `this` for JNI callbacks. super.onCreate
        // has loaded libchatter_ui by now (android.app.lib_name), so the native
        // symbol resolves.
        nativeRegisterActivity()
        // Wire the JavaVM into libwebrtc from the Java main thread so its
        // FindClass for the bundled livekit.org.webrtc classes resolves via the
        // app classloader. Doing this from Rust's native thread fails.
        nativeInitWebrtc()
    }

    /** Implemented in Rust. Initializes libwebrtc with the app's JavaVM. */
    private external fun nativeInitWebrtc()

    /**
     * The incoming-call notification's Accept button is an activity PendingIntent
     * (a broadcast receiver can't reliably foreground on Android 14+). When it
     * fires while we're already running, onNewIntent delivers it here; route the
     * accept into Rust, which runs the in-app accept flow. Cold-start (process
     * was killed) lands in onCreate before the native window exists — there the
     * full-screen intent just shows the ring screen for an in-app accept.
     */
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        if (intent.getBooleanExtra("auto_accept", false)) {
            val callId = intent.getStringExtra("incoming_call_id")
            if (!callId.isNullOrEmpty()) {
                Log.i(TAG, "accept-from-notification call_id=$callId")
                nativeAcceptIncomingCall(callId)
            }
        }
    }

    /** Implemented in Rust. Stores a GlobalRef to this Activity. */
    private external fun nativeRegisterActivity()

    /** Implemented in Rust. Runs the in-app accept flow for an incoming call. */
    private external fun nativeAcceptIncomingCall(callId: String)

    // --- File picker (Storage Access Framework) --------------------------------
    // Registered at construction (before onStart), per the ComponentActivity
    // contract that GameActivity inherits through AppCompatActivity.
    private val openDocuments =
        registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
            // SAF returns content:// URIs with transient read grants. Copy each
            // into our cache dir so Rust gets plain filesystem paths it can read.
            val paths = uris.mapNotNull { copyToCache(it) }
            nativeOnFilesPicked(paths.toTypedArray())
        }

    /** Called from Rust (JNI) to open the document chooser. */
    @Suppress("unused")
    fun launchFilePicker(mimeTypes: Array<String>) {
        val types = if (mimeTypes.isEmpty()) arrayOf("*/*") else mimeTypes
        runOnUiThread { openDocuments.launch(types) }
    }

    private fun copyToCache(uri: Uri): String? {
        return try {
            // The display name comes from another app's content provider, so it
            // is untrusted input on a path: "../../x" would resolve out of the
            // cache dir. Keep the last segment only, and fall back when what is
            // left cannot name a file.
            val name = queryDisplayName(uri)
                ?.substringAfterLast('/')
                ?.takeIf { it.isNotBlank() && it != "." && it != ".." }
                ?: "attachment_${System.nanoTime()}"
            val dir = File(cacheDir, "picked").apply { mkdirs() }
            val out = File(dir, name)
            val input = contentResolver.openInputStream(uri) ?: return null
            input.use { stream -> out.outputStream().use { stream.copyTo(it) } }
            out.absolutePath
        } catch (e: Exception) {
            Log.e(TAG, "copyToCache failed for $uri", e)
            null
        }
    }

    private fun queryDisplayName(uri: Uri): String? =
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
            ?.use { c -> if (c.moveToFirst()) c.getString(0) else null }

    /** Implemented in Rust (libchatter_ui.so). Delivers picked file paths. */
    private external fun nativeOnFilesPicked(paths: Array<String>)

    // --- Runtime permissions ---------------------------------------------------
    private val permissionRequest =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { result ->
            val granted = result.filterValues { it }.keys.toTypedArray()
            val denied = result.filterValues { !it }.keys.toTypedArray()
            nativeOnPermissionsResult(granted, denied)
        }

    /** Called from Rust (JNI). True if [permission] is already granted. */
    @Suppress("unused")
    fun hasPermission(permission: String): Boolean =
        ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED

    /** Called from Rust (JNI). Shows the system grant dialog for [permissions]. */
    @Suppress("unused")
    fun requestPermissions(permissions: Array<String>) {
        runOnUiThread { permissionRequest.launch(permissions) }
    }

    /** Implemented in Rust (libchatter_ui.so). Delivers grant results. */
    private external fun nativeOnPermissionsResult(granted: Array<String>, denied: Array<String>)

    // --- Camera capture (CameraX → I420 → Rust/LiveKit) ------------------------
    private var cameraProvider: ProcessCameraProvider? = null
    private val cameraExecutor = Executors.newSingleThreadExecutor()

    /** Called from Rust (JNI) when a call starts publishing video. */
    @Suppress("unused", "UNUSED_PARAMETER")
    fun startCamera(width: Int, height: Int, fps: Int, front: Boolean) {
        runOnUiThread {
            val future = ProcessCameraProvider.getInstance(this)
            future.addListener({
                val provider = try {
                    future.get()
                } catch (e: Exception) {
                    Log.e(TAG, "CameraProvider unavailable", e)
                    return@addListener
                }
                cameraProvider = provider
                val analysis = ImageAnalysis.Builder()
                    .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                    .setOutputImageFormat(ImageAnalysis.OUTPUT_IMAGE_FORMAT_YUV_420_888)
                    .build()
                analysis.setAnalyzer(cameraExecutor) { image ->
                    try {
                        val i420 = yuv420ToI420(image)
                        nativeOnCameraFrame(
                            i420,
                            image.width,
                            image.height,
                            image.imageInfo.rotationDegrees,
                            image.imageInfo.timestamp / 1000, // ns → µs
                        )
                    } catch (e: Exception) {
                        Log.e(TAG, "frame convert failed", e)
                    } finally {
                        image.close()
                    }
                }
                val selector = if (front) {
                    CameraSelector.DEFAULT_FRONT_CAMERA
                } else {
                    CameraSelector.DEFAULT_BACK_CAMERA
                }
                try {
                    provider.unbindAll()
                    provider.bindToLifecycle(this, selector, analysis)
                } catch (e: Exception) {
                    Log.e(TAG, "bindToLifecycle failed", e)
                }
            }, ContextCompat.getMainExecutor(this))
        }
    }

    /** Called from Rust (JNI) when video capture stops. */
    @Suppress("unused")
    fun stopCamera() {
        runOnUiThread { cameraProvider?.unbindAll() }
    }

    /** Convert a YUV_420_888 ImageProxy to tightly-packed I420 (Y, then U, V). */
    private fun yuv420ToI420(image: ImageProxy): ByteArray {
        val w = image.width
        val h = image.height
        val ySize = w * h
        val cSize = (w / 2) * (h / 2)
        val out = ByteArray(ySize + 2 * cSize)
        copyPlane(image.planes[0], w, h, out, 0)
        copyPlane(image.planes[1], w / 2, h / 2, out, ySize)
        copyPlane(image.planes[2], w / 2, h / 2, out, ySize + cSize)
        return out
    }

    private fun copyPlane(plane: ImageProxy.PlaneProxy, width: Int, height: Int, out: ByteArray, offset: Int) {
        val buffer = plane.buffer
        val rowStride = plane.rowStride
        val pixelStride = plane.pixelStride
        var outPos = offset
        if (pixelStride == 1) {
            for (r in 0 until height) {
                buffer.position(r * rowStride)
                buffer.get(out, outPos, width)
                outPos += width
            }
        } else {
            // Interleaved chroma (pixelStride 2): pick every Nth byte per row.
            val rowBuf = ByteArray(rowStride)
            for (r in 0 until height) {
                buffer.position(r * rowStride)
                val toRead = minOf(rowStride, buffer.remaining())
                buffer.get(rowBuf, 0, toRead)
                var i = 0
                var c = 0
                while (c < width) {
                    out[outPos++] = rowBuf[i]
                    i += pixelStride
                    c++
                }
            }
        }
    }

    /** Implemented in Rust (libchatter_ui.so). Delivers one I420 camera frame. */
    private external fun nativeOnCameraFrame(
        i420: ByteArray,
        width: Int,
        height: Int,
        rotation: Int,
        timestampUs: Long,
    )

    private companion object {
        const val TAG = "ChatterFilePicker"
    }
}
