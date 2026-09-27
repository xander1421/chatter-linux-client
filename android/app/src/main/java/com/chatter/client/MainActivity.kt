package com.chatter.client

import android.app.UiModeManager
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.graphics.drawable.ColorDrawable
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.OpenableColumns
import android.text.Selection
import android.util.Log
import android.view.ActionMode
import android.view.KeyEvent
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.view.inputmethod.EditorInfo
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
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
        // Until Slint's first frame the window background is all there is.
        // themes.xml resolves it from the SYSTEM night mode, so a Light app on
        // a dark phone launched on ~1.4 s of black. Paint the app's own theme,
        // mirrored here by setThemeMode, before anything draws.
        val background = themeBackground()
        window.setBackgroundDrawable(ColorDrawable(background))
        super.onCreate(savedInstanceState)
        // The window background never shows over GameActivity's SurfaceView:
        // it punches a hole in the window, and the empty surface behind it is
        // black until Slint's first frame (~1.7 s on the S9, after app_init).
        // Cover it in the theme colour until Rust reports that frame
        // (firstFrameDrawn); the timeout guarantees a missed report can never
        // leave the app hidden behind the cover.
        launchCover?.setBackgroundColor(background)
        Handler(Looper.getMainLooper()).postDelayed({ firstFrameDrawn() }, LAUNCH_COVER_TIMEOUT_MS)
        // Slint paints the theme background under the system bars, which
        // themes.xml makes transparent. Android 10+ would lay a grey contrast
        // scrim over a transparent bar; the theme background is the contrast.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            window.isStatusBarContrastEnforced = false
            window.isNavigationBarContrastEnforced = false
        }
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

    /**
     * Opt every view out of Android's default focus highlight.
     *
     * GameActivity requests focus on its full-screen SurfaceView on every window
     * focus gain (and Slint's helper adds a full-screen input View). If the
     * window is not in touch mode at that moment (the device's last input was
     * not a touch — our best explanation for why it was intermittent) View paints `selectableItemBackground`
     * (the theme's translucent white ripple) over its whole bounds, into the
     * window's HWUI layer, which sits ABOVE the SurfaceView Slint renders into:
     * a uniform grey veil over the app. View only re-evaluates that highlight in
     * onFocusChanged, and these views are focusable in touch mode so a tap never
     * takes their focus away, so the veil stayed until the process died. No view
     * here draws anything of its own; Slint draws focus itself.
     *
     * Runs for setContentView (GameActivity's surface) and addContentView
     * (Slint's input view) alike — AppCompat forwards both here.
     */
    override fun onContentChanged() {
        super.onContentChanged()
        disableFocusHighlight(window.decorView)
    }

    private fun disableFocusHighlight(view: View) {
        view.defaultFocusHighlightEnabled = false
        if (view is ViewGroup) {
            for (i in 0 until view.childCount) disableFocusHighlight(view.getChildAt(i))
        }
    }

    /**
     * Called from Rust (JNI) while a secret is on screen (the recovery codes).
     * FLAG_SECURE blanks the window — and the SurfaceView Slint draws into,
     * which is its child layer — in screenshots, screen recordings, casting and
     * the recents thumbnail. Signal does the same for its recovery material.
     */
    @Suppress("unused")
    fun setSecure(secure: Boolean) {
        runOnUiThread {
            if (secure) {
                window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
            } else {
                window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
            }
        }
    }

    /**
     * Called from Rust (JNI) whenever the app's palette changes. The app draws
     * edge-to-edge, so the status/nav bar icons sit on Slint's background: they
     * must be dark on the light palette or they vanish. WindowCompat handles
     * the pre-API-30 flags (the S9 is API 29).
     */
    @Suppress("unused")
    fun setLightSystemBars(light: Boolean) {
        runOnUiThread {
            val controller = WindowCompat.getInsetsController(window, window.decorView)
            controller.isAppearanceLightStatusBars = light
            controller.isAppearanceLightNavigationBars = light
        }
    }

    /**
     * Called from Rust (JNI) with Settings › Appearance › Theme ("system",
     * "light", "dark") at startup and on every change. Only onCreate reads it,
     * before the native side is up.
     */
    @Suppress("unused")
    fun setThemeMode(mode: String) {
        getSharedPreferences(THEME_PREFS, MODE_PRIVATE).edit().putString(THEME_MODE, mode).apply()
        // API 31+: the system draws its starting window from the app's night
        // mode, before onCreate runs at all. Persisted by the system; uiMode is
        // in the manifest's configChanges, so this does not recreate us.
        // Below 31 the starting window follows the SYSTEM night mode and no
        // app setting can reach it; accepted, as Signal does (OPEN-ITEMS §124).
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            getSystemService(UiModeManager::class.java).setApplicationNightMode(
                when (mode) {
                    "light" -> UiModeManager.MODE_NIGHT_NO
                    "dark" -> UiModeManager.MODE_NIGHT_YES
                    else -> UiModeManager.MODE_NIGHT_AUTO
                }
            )
        }
    }

    /** The SurfaceView Slint draws into, covered until its first frame. */
    private var launchCover: View? = null

    /** Called from Rust (JNI) after Slint's first frame; idempotent. */
    @Suppress("unused")
    fun firstFrameDrawn() {
        runOnUiThread {
            launchCover?.background = null
            launchCover = null
        }
    }

    /** colors.xml's window_background for the mirrored theme; System = uiMode. */
    private fun themeBackground(): Int {
        val night = when (getSharedPreferences(THEME_PREFS, MODE_PRIVATE).getString(THEME_MODE, "system")) {
            "light" -> Configuration.UI_MODE_NIGHT_NO
            "dark" -> Configuration.UI_MODE_NIGHT_YES
            else -> resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK
        }
        val config = Configuration(resources.configuration)
        config.uiMode = (config.uiMode and Configuration.UI_MODE_NIGHT_MASK.inv()) or night
        return createConfigurationContext(config).getColor(R.color.window_background)
    }

    // --- Call audio routing (earpiece / speaker / headset) -------------------
    // Called from Rust (JNI, platform/android/audio_route.rs); CallAudioRouter
    // hops to the main looper itself.
    private val callAudio by lazy { CallAudioRouter(this) }

    @Suppress("unused")
    fun startCallAudio() = callAudio.start()

    @Suppress("unused")
    fun stopCallAudio() = callAudio.stop()

    @Suppress("unused")
    fun applyAudioRoute(route: Int) = callAudio.apply(route)

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
        val callId = intent.getStringExtra("incoming_call_id")
        if (callId.isNullOrEmpty()) return
        if (intent.getBooleanExtra("auto_accept", false)) {
            Log.i(TAG, "accept-from-notification call_id=$callId")
            nativeAcceptIncomingCall(callId)
        } else {
            // The notification body: bring the ring screen back if Back hid it.
            nativeShowIncomingCall(callId)
        }
    }

    /** Implemented in Rust. Stores a GlobalRef to this Activity. */
    private external fun nativeRegisterActivity()

    /** Implemented in Rust. Runs the in-app accept flow for an incoming call. */
    private external fun nativeAcceptIncomingCall(callId: String)

    /** Implemented in Rust. Re-shows a ringing call's screen that Back hid. */
    private external fun nativeShowIncomingCall(callId: String)

    // The Slint helper owns this floating selection toolbar, but GameActivity's
    // surface owns pre-IME BACK. Finish the toolbar before hiding the keyboard;
    // otherwise Samsung consumes BACK without ever notifying the helper view.
    private var textSelectionMode: ActionMode? = null

    override fun onActionModeStarted(mode: ActionMode) {
        super.onActionModeStarted(mode)
        if (mode.tag == "slint-text-selection") textSelectionMode = mode
    }

    override fun onActionModeFinished(mode: ActionMode) {
        if (textSelectionMode === mode) textSelectionMode = null
        super.onActionModeFinished(mode)
    }

    // --- Back key --------------------------------------------------------------
    /**
     * Back with the soft keyboard up closes the keyboard, decided here, BEFORE
     * the key is offered to the IME.
     *
     * The IME's own Back handling is what normally does this, and on the S25
     * (Android 16, Samsung Keyboard) it did not: logcat showed the IME's
     * key-down step (`handleBack: mShowInputRequested=true, doIt=false`) —
     * which CONSUMES the press — and nothing after it, so the keyboard stayed
     * up and the app never saw Back either. The show flags are not the cause:
     * the whole path already asks for an implicit show
     * (androidwindowadapter.rs `show_soft_input(true)` → android-activity
     * `ShowImeFlags_SHOW_IMPLICIT` → GameTextInput `setSoftKeyboardActive` →
     * `imm.showSoftInput(view, SHOW_IMPLICIT)`), and `mShowInputRequested` is
     * true for any shown keyboard.
     *
     * `onKeyPreIme` on the focused view runs before the IME stage (the
     * pattern SearchView and AutoCompleteTextView use for the same job), so
     * this no longer depends on the IME finishing its half. Both halves are
     * consumed so Rust's `back-scope` (ui/main.slint) does not also pop a
     * layer: the first Back hides the keyboard, the next one navigates — what
     * the S9 already did. With no keyboard up, Back is untouched.
     */
    override fun createSurfaceView(): InputEnabledSurfaceView =
        BackHidesImeSurfaceView().also { launchCover = it }

    private inner class BackHidesImeSurfaceView : InputEnabledSurfaceView(this@MainActivity) {
        /** The current Back press started while the keyboard was up. */
        private var backHidesIme = false
        private var backSelectionMode: ActionMode? = null

        override fun onCreateInputConnection(outAttrs: EditorInfo): com.google.androidgamesdk.gametextinput.InputConnection? {
            val connection = super.onCreateInputConnection(outAttrs) ?: return null
            // GameTextInput 4.4 setState changes the Editable then restarts input,
            // but does not refresh EditorInfo's initial selection. An IME can
            // then restore stale offsets after toolbar Select all (RC27 D2).
            // Read the actual UTF-16 spans whenever Android opens the connection.
            val editable = connection.editable
            outAttrs.initialSelStart = Selection.getSelectionStart(editable)
            outAttrs.initialSelEnd = Selection.getSelectionEnd(editable)
            return connection
        }

        override fun onKeyPreIme(keyCode: Int, event: KeyEvent): Boolean {
            if (keyCode != KeyEvent.KEYCODE_BACK) return super.onKeyPreIme(keyCode, event)
            when (event.action) {
                KeyEvent.ACTION_DOWN -> {
                    if (event.repeatCount == 0) {
                        backSelectionMode = textSelectionMode
                        backHidesIme = backSelectionMode == null && ViewCompat.getRootWindowInsets(this)
                            ?.isVisible(WindowInsetsCompat.Type.ime()) == true
                    }
                    if (backSelectionMode != null || backHidesIme) return true
                }
                KeyEvent.ACTION_UP -> if (backSelectionMode != null) {
                    val mode = backSelectionMode
                    backSelectionMode = null
                    if (!event.isCanceled) mode?.finish()
                    return true
                } else if (backHidesIme) {
                    backHidesIme = false
                    if (!event.isCanceled) {
                        WindowCompat.getInsetsController(window, this)
                            .hide(WindowInsetsCompat.Type.ime())
                    }
                    return true
                }
            }
            return super.onKeyPreIme(keyCode, event)
        }
    }

    /**
     * Called from Rust (JNI) when Back is pressed with nothing left to pop.
     *
     * `moveTaskToBack`, not `finish()`: the user is leaving the app, not ending
     * their session. Finishing would tear down the Activity and the native
     * process state behind it (open streams, decrypted session), so returning
     * would be a cold start. Must run on the UI thread — the Back press arrives
     * on Rust's native android_main thread.
     */
    @Suppress("unused")
    fun moveAppToBack() {
        runOnUiThread { moveTaskToBack(true) }
    }

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

    /** Android 11 and older require system Location for BLE discovery. */
    @Suppress("unused")
    fun meshLocationEnabled(): Boolean {
        if (android.os.Build.VERSION.SDK_INT >= 31) return true
        val manager = getSystemService(android.content.Context.LOCATION_SERVICE) as? android.location.LocationManager
            ?: return false
        return if (android.os.Build.VERSION.SDK_INT >= 28) manager.isLocationEnabled
        else manager.isProviderEnabled(android.location.LocationManager.GPS_PROVIDER) ||
            manager.isProviderEnabled(android.location.LocationManager.NETWORK_PROVIDER)
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

    /** Called from Rust (JNI, platform/android/qr_scanner.rs). See QrScanner. */
    @Suppress("unused")
    fun scanQrCode() {
        runOnUiThread { QrScanner(this).show() }
    }

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
        const val THEME_PREFS = "chatter_theme"
        const val THEME_MODE = "mode"
        const val LAUNCH_COVER_TIMEOUT_MS = 5_000L
    }
}
