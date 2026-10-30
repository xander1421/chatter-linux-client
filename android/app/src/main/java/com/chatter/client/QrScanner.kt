package com.chatter.client

import android.app.Dialog
import android.graphics.Color
import android.util.Log
import android.view.Gravity
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.FrameLayout
import android.widget.TextView
import android.widget.Toast
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.core.content.ContextCompat
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.MultiFormatReader
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.ReaderException
import com.google.zxing.common.HybridBinarizer
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * One-shot in-app QR scanner: a full-screen dialog with the back camera's
 * preview, decoding each frame's luma plane with ZXing core (Apache-2.0, pure
 * Java — no Play services). Reports exactly once to Rust through
 * [nativeOnQrScanned]: the decoded text, or null when the user backs out or the
 * camera cannot open. Rust asks for CAMERA before opening this; it only decodes,
 * and the caller shows what was scanned and asks before acting on it.
 */
class QrScanner(private val activity: MainActivity) {
    private val done = AtomicBoolean(false)
    private val executor = Executors.newSingleThreadExecutor()
    private val preview = Preview.Builder().build()
    private val analysis = ImageAnalysis.Builder()
        .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
        .build()
    private val reader = MultiFormatReader().apply {
        setHints(mapOf(DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE)))
    }
    private val dialog = Dialog(activity, android.R.style.Theme_Black_NoTitleBar_Fullscreen)
    private var provider: ProcessCameraProvider? = null

    /** Must run on the UI thread. */
    fun show() {
        val density = activity.resources.displayMetrics.density
        val previewView = PreviewView(activity)
        val hint = TextView(activity).apply {
            text = "Point the camera at the QR code"
            setTextColor(Color.WHITE)
            setShadowLayer(4f, 0f, 0f, Color.BLACK)
            textSize = 18f
            gravity = Gravity.CENTER
            val pad = (24 * density).toInt()
            setPadding(pad, pad * 2, pad, pad)
        }
        val cancel = Button(activity).apply {
            text = "Cancel"
            setOnClickListener { dialog.cancel() }
        }
        dialog.setContentView(FrameLayout(activity).apply {
            addView(previewView, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
            addView(hint, FrameLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT, Gravity.TOP))
            // Clear of the gesture/nav bar, which the fullscreen theme leaves drawn.
            addView(cancel, FrameLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT,
                Gravity.BOTTOM or Gravity.CENTER_HORIZONTAL).apply { bottomMargin = (64 * density).toInt() })
        })
        // Back and the Cancel button both land here.
        dialog.setOnCancelListener { finish(null) }
        dialog.show()

        preview.setSurfaceProvider(previewView.surfaceProvider)
        analysis.setAnalyzer(executor) { image -> decode(image) }
        val future = ProcessCameraProvider.getInstance(activity)
        future.addListener({
            if (done.get()) return@addListener
            try {
                val p = future.get()
                provider = p
                p.bindToLifecycle(activity, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis)
            } catch (e: Exception) {
                // No camera, or it is held by an ongoing video call.
                Log.e(TAG, "scanner camera bind failed", e)
                Toast.makeText(activity, "Camera unavailable", Toast.LENGTH_LONG).show()
                finish(null)
            }
        }, ContextCompat.getMainExecutor(activity))
    }

    private fun decode(image: ImageProxy) {
        try {
            if (done.get()) return
            // YUV_420_888's Y plane is 8-bit luma with pixelStride 1; rows are
            // rowStride apart, which ZXing takes as the data width.
            val plane = image.planes[0]
            val buf = plane.buffer
            val bytes = ByteArray(buf.remaining()).also { buf.get(it) }
            val source = PlanarYUVLuminanceSource(
                bytes, plane.rowStride, image.height, 0, 0, image.width, image.height, false,
            )
            val text = try {
                reader.decodeWithState(BinaryBitmap(HybridBinarizer(source))).text
            } catch (e: ReaderException) {
                null
            } finally {
                reader.reset()
            }
            if (text != null) finish(text)
        } catch (e: Exception) {
            Log.e(TAG, "scanner frame decode failed", e)
        } finally {
            image.close()
        }
    }

    private fun finish(text: String?) {
        if (!done.compareAndSet(false, true)) return
        activity.runOnUiThread {
            provider?.unbind(preview, analysis)
            dialog.dismiss()
            executor.shutdown()
        }
        nativeOnQrScanned(text)
    }

    /** Implemented in Rust (platform/android/qr_scanner.rs). */
    private external fun nativeOnQrScanned(text: String?)

    private companion object {
        const val TAG = "ChatterQrScanner"
    }
}
