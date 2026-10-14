package com.chatter.client

import android.app.UiModeManager
import android.app.KeyguardManager
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
import android.view.Surface
import android.view.SurfaceHolder
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
import com.google.androidgamesdk.ChatterTextInputGate
import java.io.File
import java.nio.file.Files
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
    @Suppress("unused")
    fun isUpdateNetworkMetered(): Boolean {
        val manager = getSystemService(android.content.Context.CONNECTIVITY_SERVICE)
            as android.net.ConnectivityManager
        return manager.isActiveNetworkMetered
    }

    @Suppress("unused")
    fun installVerifiedUpdate(path: String): Boolean = UpdateInstaller.start(this, path)

    @Suppress("unused")
    fun beginViewerMediaExport(name: String, mime: String, copy: Boolean): LongArray? =
        ViewerMediaExport.begin(this, name, mime, copy)

    @Suppress("unused")
    fun beginViewerPdfExport(name: String, mime: String, copy: Boolean): LongArray? =
        ViewerMediaExport.begin(this, name, mime, copy, pdf = true)

    @Suppress("unused")
    @androidx.annotation.Keep
    fun clearViewerSession(revision: Long): Boolean =
        ViewerMediaExport.sessionChanged(this, revision)

    fun finishViewerMediaExport(token: Long, success: Boolean): Boolean =
        ViewerMediaExport.finish(this, token, success)

    @Suppress("unused")
    fun openViewerDownloads() {
        runOnUiThread {
            try { ViewerMediaExport.openDownloads(this) }
            catch (_: Exception) { android.widget.Toast.makeText(this, "Downloads unavailable", android.widget.Toast.LENGTH_SHORT).show() }
        }
    }


    // Called only after an explicit tap; duplicate validation at the native edge.
    fun openWebLink(value: String) {
        runOnUiThread {
            try {
                val uri = Uri.parse(value)
                if ((uri.scheme != "https" && uri.scheme != "http") || uri.host.isNullOrEmpty()
                    || uri.userInfo != null || value.any { it.isWhitespace() || it.isISOControl() || it == '\\' }) return@runOnUiThread
                startActivity(Intent(Intent.ACTION_VIEW, uri).addCategory(Intent.CATEGORY_BROWSABLE))
            } catch (_: Exception) {
                android.widget.Toast.makeText(this, "Unable to open browser", android.widget.Toast.LENGTH_SHORT).show()
            }
        }
    }

    fun showVideoError() {
        runOnUiThread { android.widget.Toast.makeText(this, "Unable to play this video", android.widget.Toast.LENGTH_LONG).show() }
    }

    private external fun nativeVideoTokenIsCurrent(token: Long): Boolean

    fun playVideo(token: Long, label: String) {
        runOnUiThread {
            try {
                if (!nativeVideoTokenIsCurrent(token)) return@runOnUiThread
                startActivity(Intent(this, VideoPlaybackActivity::class.java)
                    .putExtra("video-token", token).putExtra("video-title", label))
            } catch (_: Exception) {
                nativeReleasePendingVideo(token)
                android.widget.Toast.makeText(this, "Unable to open video player", android.widget.Toast.LENGTH_SHORT).show()
            }
        }
    }
    private external fun nativeReleasePendingVideo(token: Long)

    override fun onCreate(savedInstanceState: Bundle?) {
        ViewerMediaExport.startup(this)
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
        dispatchMessageNotification(intent)
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
        dispatchMessageNotification(intent)
        // Public launcher intents are untrusted. The action is routing only;
        // the native boundary authenticates and consumes the random capability
        // before notification cancellation or any call/UI effects.
        val action = intent.action ?: return
        if (action != "com.chatter.client.CALL_ACCEPT" &&
            action != "com.chatter.client.CALL_OPEN" &&
            action != "com.chatter.client.CALL_FULL_SCREEN") return
        try {
            val callId = intent.getStringExtra("incoming_call_id") ?: return
            val capability = intent.getStringExtra("call_notification_capability") ?: return
            if (callId.isEmpty() || capability.isEmpty()) return
            when (action) {
                "com.chatter.client.CALL_ACCEPT" -> {
                    if (!intent.getBooleanExtra("auto_accept", false)) return
                    nativeAcceptIncomingCall(callId, capability)
                }
                else -> {
                    if (intent.getBooleanExtra("auto_accept", false)) return
                    nativeShowIncomingCall(callId, action, capability)
                }
            }
        } catch (_: RuntimeException) {
            // Malformed public extras are neither consent nor a reason to crash.
        }
    }

    private fun dispatchMessageNotification(intent: Intent) {
        if (intent.action != "com.chatter.client.MESSAGE_OPEN" ||
            getSystemService(KeyguardManager::class.java)?.isDeviceLocked == true) {
            nativeNotificationOpened("")
            return
        }
        try {
            val key = intent.getStringExtra("notification_conversation_key") ?: ""
            if (key.isEmpty() || (key.length == 32 && key.all { it in '0'..'9' || it in 'a'..'f' })) {
                nativeNotificationOpened(key)
            }
        } catch (_: RuntimeException) {
            // Public launcher extras are untrusted.
        }
    }

    private external fun nativeNotificationOpened(key: String)

    /** Called by core after login/start and before sign-out teardown. */
    fun setMessagingSignedIn(signedIn: Boolean) {
        runOnUiThread {
            messagingSignedIn = signedIn
            updateMessagingService()
        }
    }

    private fun updateMessagingService() {
        if (messagingSignedIn && messagingBackground) {
            MessagingService.start(this)
        } else {
            MessagingService.stop(this)
        }
    }

    override fun onPause() {
        // Start while the Activity still has foreground-start eligibility.
        messagingBackground = true
        updateMessagingService()
        super.onPause()
    }

    override fun onResume() {
        super.onResume()
        messagingBackground = false
        updateMessagingService()
    }

    /** Implemented in Rust. Stores a GlobalRef to this Activity. */
    private external fun nativeRegisterActivity()

    /** Implemented in Rust. Runs the in-app accept flow for an incoming call. */
    private external fun nativeAcceptIncomingCall(callId: String, capability: String)

    /** Implemented in Rust. Re-shows a ringing call's screen that Back hid. */
    private external fun nativeShowIncomingCall(callId: String, action: String, capability: String)

    /** Called from Rust (JNI, any thread): Slint's state for the IME's field. UTF-16 offsets. */
    @Suppress("unused")
    fun textInputPush(text: String, selStart: Int, selEnd: Int, compStart: Int, compEnd: Int, owner: Long, generation: Long) =
        ChatterTextInputGate.push(text, selStart, selEnd, compStart, compEnd, owner, generation)

    /** Called from Rust (JNI, any thread): a send was accepted, the field's native copy is consumed. */
    @Suppress("unused")
    fun textInputConsume(owner: Long, generation: Long) = ChatterTextInputGate.publishReset(owner, generation)

    /** Implemented in Rust. Back hid the keyboard: drop Slint's text focus too. */
    private external fun nativeImeHiddenByBack()

    /** Implemented in Rust. The IME's action key, delivered to Slint as Enter. */
    private external fun nativeImeEditorAction()

    /**
     * The soft keyboard's action key (Done — GameActivity's default
     * `IME_ACTION_DONE`) and a single-line Enter both land here, via
     * GameTextInput's `performEditorAction` / `processKeyEvent`. GameActivity
     * queues it as an editor action that android-activity surfaces as
     * `InputEvent::TextAction`, which Slint's backend drops
     * (i-slint-backend-android-activity 1.17.1 `process_inputs`): so a
     * TextInput's `accepted` never fired on a phone (New Channel's name → Description).
     * Hand it to Slint as the Enter key instead, which is what its
     * native-activity backend gets from the same key (BaseInputConnection
     * sends KEYCODE_ENTER). Not passed to super: nothing reads the queued action.
     */
    override fun onEditorAction(action: Int) {
        nativeImeEditorAction()
    }

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
     * consumed so Rust's `back-scope` (ui/app.slint) does not also pop a
     * layer: the first Back hides the keyboard, the next one navigates — what
     * the S9 already did. With no keyboard up, Back is untouched.
     */
    override fun createSurfaceView(): InputEnabledSurfaceView =
        BackHidesImeSurfaceView().also { launchCover = it }

    // --- Refresh rate ----------------------------------------------------------
    // An app that does not vote is left at the panel's default rate, which on a
    // 120 Hz LTPO phone is often 60 Hz, so a fling never reaches the rate the
    // hardware offers. A vote held all the time would keep the panel at its top
    // rate over an idle chat, so the Slint surface votes only while the UI
    // moves: Rust (run_event_loop in the vendored android backend) calls
    // setUiMoving(true) when an animation, fling or touch drag starts and
    // setUiMoving(false) once the UI has been at rest for a moment.

    /** Fastest rate the panel offers at its current resolution; 0 below API 30. */
    @Volatile private var panelRefreshRate = 0f

    /** UI thread only. */
    private var uiMoving = false
    private var frameRateSurface: Surface? = null

    override fun surfaceCreated(holder: SurfaceHolder) {
        super.surfaceCreated(holder)
        trackFrameRateSurface(holder.surface)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        super.surfaceChanged(holder, format, width, height)
        // Rotation, a fold or a resolution change can change the modes on offer.
        trackFrameRateSurface(holder.surface)
    }

    private fun trackFrameRateSurface(surface: Surface) {
        frameRateSurface = surface
        panelRefreshRate = fastestRefreshRate()
        applyFrameRate()
    }

    /**
     * Called from Rust (JNI) on each transition between moving and at rest, never
     * per frame. Returns the panel's top rate (0 if unknown) so Rust can floor
     * its frame loop at half a vsync.
     */
    @Suppress("unused")
    fun setUiMoving(moving: Boolean): Float {
        runOnUiThread {
            uiMoving = moving
            applyFrameRate()
        }
        return panelRefreshRate
    }

    /** Highest refresh rate at the display's current resolution; 0 below API 30. */
    private fun fastestRefreshRate(): Float {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return 0f
        val display = display ?: return 0f
        val current = display.mode
        return display.supportedModes
            .filter { it.physicalWidth == current.physicalWidth && it.physicalHeight == current.physicalHeight }
            .maxOfOrNull { it.refreshRate } ?: 0f
    }

    /**
     * Vote the panel's top rate while moving; clear the vote at rest so an LTPO
     * panel can drop back down. API 36 asks UI (scrolling, flings) to vote
     * FRAME_RATE_COMPATIBILITY_AT_LEAST; DEFAULT is the game compatibility,
     * which is all API 30-35 offers.
     */
    private fun applyFrameRate() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
        val surface = frameRateSurface?.takeIf { it.isValid } ?: return
        val rate = if (uiMoving) panelRefreshRate else 0f
        val compatibility = if (rate > 0f && Build.VERSION.SDK_INT >= API_36) {
            FRAME_RATE_COMPATIBILITY_AT_LEAST
        } else {
            Surface.FRAME_RATE_COMPATIBILITY_DEFAULT
        }
        surface.setFrameRate(rate, compatibility)
    }

    // GatedSurfaceView puts the producer gate (ChatterTextInputGate) in as the
    // view's InputConnection while the view is constructed, before GameActivity
    // registers it with native code: an accepted send consumes the IME's copy
    // of the composer in order with the keyboard's own operations.
    private inner class BackHidesImeSurfaceView : ChatterTextInputGate.GatedSurfaceView(this@MainActivity) {
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
                        // Slint never hears about that hide: its TextInput keeps
                        // focus and the keyboard can come straight back.
                        nativeImeHiddenByBack()
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
        var copiedFile: File? = null
        return try {
            // The display name comes from another app's content provider, so it
            // is untrusted input on a path: "../../x" would resolve out of the
            // cache dir. Keep the last segment only, and fall back when what is
            // left cannot name a file.
            val name = queryDisplayName(uri)
                ?.substringAfterLast('/')
                ?.takeIf { it.isNotBlank() && it != "." && it != ".." }
                ?: "attachment_${System.nanoTime()}"
            val input = contentResolver.openInputStream(uri) ?: return null
            input.use { stream ->
                val dir = File(cacheDir, "picked").apply { mkdirs() }
                // Display names are not identities: two providers (or two
                // successive picks) can both return "image.jpg". Keep the
                // name for the attachment UI, but never overwrite a pending
                // attachment's bytes with a later selection.
                val selectionDir = Files.createTempDirectory(dir.toPath(), "selection_").toFile()
                val out = File(selectionDir, name)
                copiedFile = out
                out.outputStream().use { stream.copyTo(it) }
                out.absolutePath
            }
        } catch (_: Exception) {
            copiedFile?.delete()
            copiedFile?.parentFile?.delete()
            // Provider URIs and exception messages can identify private files.
            Log.e(TAG, "Attachment copy failed")
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
        // Account/process state survives Activity recreation.
        @Volatile private var messagingSignedIn = false
        private var messagingBackground = false
        const val TAG = "ChatterFilePicker"
        const val THEME_PREFS = "chatter_theme"
        const val THEME_MODE = "mode"
        const val LAUNCH_COVER_TIMEOUT_MS = 5_000L

        // compileSdk is 35; these are Build.VERSION_CODES.BAKLAVA and
        // Surface.FRAME_RATE_COMPATIBILITY_AT_LEAST from the API 36 SDK.
        const val API_36 = 36
        const val FRAME_RATE_COMPATIBILITY_AT_LEAST = 2
    }
}
