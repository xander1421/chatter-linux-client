package com.chatter.client

import android.app.Activity
import android.content.res.ColorStateList
import android.content.res.Configuration
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.media.MediaDataSource
import android.media.MediaPlayer
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.TextUtils
import android.util.Log
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.SeekBar
import android.widget.TextView
import android.widget.Toast
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

/** Private decoder surface. Plaintext is read from Rust memory/authenticated ranges,
 * never a URL, public file, temporary plaintext file, or an external player. */
class VideoPlaybackActivity : Activity(), SurfaceHolder.Callback {
    // Only fixed labels are logged: never source identifiers or exception contents.
    private enum class FailureStage {
        SOURCE_VALIDATION, PLAYER_CREATE, AUDIO_ATTRIBUTES, DISPLAY, DATA_SOURCE,
        VIDEO_SCALING, LISTENERS, PREPARE_ASYNC, PLAYER_ERROR, PLAYER_IO_PREPARE,
        PLAYER_IO_PLAYBACK, PAUSE, AUDIO_FOCUS, START, SEEK
    }
    private enum class SourceFailure { NONE, INVALID_READ, SIZE_UNAVAILABLE, READ_UNAVAILABLE }
    // MediaDataSource callbacks run on framework threads, independently of the UI.
    private val sourceFailure = AtomicReference(SourceFailure.NONE)
    private val sourceRead = AtomicBoolean(false)
    private val sourceEof = AtomicBoolean(false)
    private val sourceClosed = AtomicBoolean(false)
    private var token = 0L
    private var player: MediaPlayer? = null
    private var prepared = false
    @Volatile private var released = false
    private var foreground = false
    private lateinit var frame: FrameLayout
    private lateinit var videoView: SurfaceView
    private lateinit var header: LinearLayout
    private lateinit var footer: LinearLayout
    private lateinit var playButton: ImageButton
    private lateinit var muteButton: ImageButton
    private lateinit var elapsed: TextView
    private lateinit var duration: TextView
    private lateinit var seek: SeekBar
    private lateinit var position: TextView
    private lateinit var previousButton: ImageButton
    private lateinit var nextButton: ImageButton
    private var seeking = false
    private var muted = false
    private var videoWidth = 0
    private var videoHeight = 0
    private lateinit var audioManager: AudioManager
    private lateinit var focus: AudioFocusRequest
    private var hasFocus = false
    private var resumeOnFocusGain = false
    private val clock = Handler(Looper.getMainLooper())
    private val clockTick = object : Runnable {
        override fun run() {
            updatePlayback()
            if (foreground && !released) clock.postDelayed(this, 250)
        }
    }
    private external fun nativeVideoTokenIsCurrent(token: Long): Boolean
    private external fun nativeVideoSize(token: Long): Long
    private external fun nativeReadVideo(token: Long, position: Long, buffer: ByteArray, offset: Int, count: Int): Int
    private external fun nativeReleaseVideo(token: Long)
    private external fun nativeViewerAction(token: Long, action: Int)
    private external fun nativeViewerCanNavigate(token: Long, delta: Int): Boolean
    private external fun nativeViewerPosition(token: Long): Long

    private fun dp(value: Int) = (value * resources.displayMetrics.density + 0.5f).toInt()

    private val dark: Boolean by lazy {
        when (getSharedPreferences("chatter_theme", MODE_PRIVATE).getString("mode", "system")) {
            "dark" -> true
            "light" -> false
            else -> resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK != Configuration.UI_MODE_NIGHT_NO
        }
    }
    private val ink: Int get() = Color.parseColor(if (dark) "#eef3f8" else "#101a25")
    private val mutedInk: Int get() = Color.parseColor(if (dark) "#97a8ba" else "#556677")
    private val primary: Int get() = Color.parseColor(if (dark) "#0d9488" else "#0f766e")

    private fun chromeBackground(): GradientDrawable = GradientDrawable().apply {
        shape = GradientDrawable.RECTANGLE
        cornerRadius = dp(16).toFloat() // Spacing.radius-lg
        setColor(Color.parseColor(if (dark) "#661a2635" else "#75ffffff")) // Materials.regular
        setStroke(dp(1), Color.parseColor(if (dark) "#40ffffff" else "#301a2c3e"))
    }

    private fun iconButton(icon: Int, label: String, action: () -> Unit): ImageButton = ImageButton(this).apply {
        setImageResource(icon)
        setColorFilter(ink)
        background = GradientDrawable().apply {
            shape = GradientDrawable.OVAL
            setColor(Color.parseColor(if (icon == R.drawable.ic_video_close) {
                if (dark) "#521a2635" else "#61ffffff" // Materials.thin
            } else {
                if (dark) "#263444" else "#dfe6ed" // Colors.fill-secondary
            }))
        }
        contentDescription = label
        scaleType = ImageView.ScaleType.CENTER_INSIDE
        val inset = if (icon == R.drawable.ic_video_close) 11 else 15
        setPadding(dp(inset), dp(inset), dp(inset), dp(inset))
        setOnClickListener { action() }
    }

    private fun timeLabel(): TextView = TextView(this).apply {
        setTextColor(mutedInk)
        textSize = 12f
        gravity = Gravity.CENTER
        setSingleLine(true)
    }

    private fun makeChrome(filename: String) {
        header = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            background = chromeBackground()
            setPadding(dp(12), 0, 0, 0)
        }
        val name = TextView(this).apply {
            text = filename
            setTextColor(ink)
            textSize = 16f
            typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
            maxLines = 1
            ellipsize = TextUtils.TruncateAt.END
            contentDescription = filename
        }
        header.addView(name, LinearLayout.LayoutParams(0, -2, 1f))
        header.addView(iconButton(R.drawable.ic_video_close, "Close") { finish() }, LinearLayout.LayoutParams(dp(48), dp(48)))
        frame.addView(header, FrameLayout.LayoutParams(dp(300), dp(48), Gravity.TOP or Gravity.START))

        footer = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            background = chromeBackground()
        }
        val playback = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(0, 0, dp(8), 0)
        }
        playButton = iconButton(R.drawable.ic_video_pause, "Pause video") { togglePlayback() }
        playback.addView(playButton, LinearLayout.LayoutParams(dp(48), dp(48)))
        elapsed = timeLabel().apply { text = "0:00" }
        playback.addView(elapsed, LinearLayout.LayoutParams(-2, dp(48)))
        seek = SeekBar(this).apply {
            progressTintList = ColorStateList.valueOf(primary)
            thumbTintList = ColorStateList.valueOf(primary)
            progressBackgroundTintList = ColorStateList.valueOf(Color.parseColor(if (dark) "#3346586a" else "#33556677"))
            max = 1000
            contentDescription = "Seek video"
            setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
                override fun onStartTrackingTouch(bar: SeekBar) { seeking = true }
                override fun onStopTrackingTouch(bar: SeekBar) {
                    seeking = false
                    seekTo((durationMs().toLong() * bar.progress / 1000).toInt())
                    updatePlayback()
                }
                override fun onProgressChanged(bar: SeekBar, value: Int, fromUser: Boolean) {}
            })
        }
        playback.addView(seek, LinearLayout.LayoutParams(0, dp(48), 1f))
        duration = timeLabel().apply { text = "0:00" }
        playback.addView(duration, LinearLayout.LayoutParams(-2, dp(48)))
        muteButton = iconButton(R.drawable.ic_video_volume, "Mute video") { toggleMute() }
        playback.addView(muteButton, LinearLayout.LayoutParams(dp(48), dp(48)))
        footer.addView(playback, LinearLayout.LayoutParams(-1, dp(48)))

        val actions = LinearLayout(this).apply { gravity = Gravity.CENTER }
        fun action(icon: Int, label: String, code: Int, enabled: Boolean = true): ImageButton {
            val button = iconButton(icon, label) {
                nativeViewerAction(token, code)
                if (code == -1 || code == 1) finish()
            }.apply {
                isEnabled = enabled
                alpha = if (enabled) 1f else 0.38f
            }
            actions.addView(button, LinearLayout.LayoutParams(dp(48), dp(48)))
            return button
        }
        action(R.drawable.ic_video_download, "Download", 2)
        action(R.drawable.ic_video_folder, "Open downloads", 3)
        previousButton = action(R.drawable.ic_video_chevron_left, "Previous media", -1, nativeViewerCanNavigate(token, -1))
        position = timeLabel().apply {
            textSize = 14f
            val packed = nativeViewerPosition(token)
            val index = packed ushr 32
            val total = packed and 0xffffffffL
            text = if (index > 0 && total > 0) "$index / $total" else ""
        }
        actions.addView(position, LinearLayout.LayoutParams(-2, dp(48)))
        nextButton = action(R.drawable.ic_video_chevron_right, "Next media", 1, nativeViewerCanNavigate(token, 1))
        footer.addView(actions, LinearLayout.LayoutParams(-1, dp(48)))
        frame.addView(footer, FrameLayout.LayoutParams(dp(300), dp(96), Gravity.TOP or Gravity.START))
    }

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        // The media scrim is dark in both app themes; keep system-bar icons legible.
        if (Build.VERSION.SDK_INT >= 30) {
            window.insetsController?.setSystemBarsAppearance(
                0,
                WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS or
                    WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
            )
        } else {
            window.decorView.systemUiVisibility = window.decorView.systemUiVisibility and
                View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR.inv() and View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR.inv()
        }
        token = intent.getLongExtra("video-token", 0)
        if (!nativeVideoTokenIsCurrent(token)) { finish(); return }
        title = intent.getStringExtra("video-title") ?: "Video"
        audioManager = getSystemService(AudioManager::class.java)
        focus = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
            .setAudioAttributes(AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
                .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE).build())
            .setOnAudioFocusChangeListener { change ->
                if (released) return@setOnAudioFocusChangeListener
                when (change) {
                    AudioManager.AUDIOFOCUS_GAIN -> {
                        hasFocus = true
                        val resume = resumeOnFocusGain
                        resumeOnFocusGain = false
                        if (resume && prepared && !released) start()
                    }
                    AudioManager.AUDIOFOCUS_LOSS_TRANSIENT,
                    AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK -> {
                        val resume = resumeOnFocusGain || isPlaying()
                        pausePlayer()
                        resumeOnFocusGain = resume
                        hasFocus = false
                    }
                    AudioManager.AUDIOFOCUS_LOSS -> {
                        resumeOnFocusGain = false
                        pausePlayer()
                        abandonFocus()
                    }
                }
            }.build()
        frame = FrameLayout(this).apply {
            setBackgroundColor(Color.parseColor(if (dark) "#e602050a" else "#e00b1420")) // Materials.scrim-media
            setOnApplyWindowInsetsListener { view: View, insets: WindowInsets ->
                val bars = if (Build.VERSION.SDK_INT >= 30) {
                    insets.getInsets(WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout())
                } else null
                if (bars != null) view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
                else view.setPadding(insets.systemWindowInsetLeft, insets.systemWindowInsetTop,
                    insets.systemWindowInsetRight, insets.systemWindowInsetBottom)
                view.post { fitSurface() }
                insets
            }
        }
        videoView = SurfaceView(this).apply { setOnClickListener { togglePlayback() } }
        frame.addView(videoView, FrameLayout.LayoutParams(-1, -1, Gravity.TOP or Gravity.START))
        makeChrome(title.toString())
        setContentView(frame)
        frame.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> fitSurface() }
        videoView.holder.addCallback(this)
        if (token <= 0 || nativeVideoSize(token) <= 0) fail(FailureStage.SOURCE_VALIDATION)
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        if (released || player != null) return
        var stage = FailureStage.PLAYER_CREATE
        try {
            val media = MediaPlayer()
            player = media
            stage = FailureStage.AUDIO_ATTRIBUTES
            media.setAudioAttributes(AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
                .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE).build())
            stage = FailureStage.DISPLAY
            media.setDisplay(holder)
            stage = FailureStage.DATA_SOURCE
            media.setDataSource(object : MediaDataSource() {
                override fun getSize(): Long {
                    val length = nativeVideoSize(token)
                    if (length < 0) sourceFailure.compareAndSet(SourceFailure.NONE, SourceFailure.SIZE_UNAVAILABLE)
                    return length
                }
                override fun readAt(position: Long, buffer: ByteArray, offset: Int, size: Int): Int {
                    if (position < 0 || offset < 0 || size < 0 || offset > buffer.size || size > buffer.size - offset) {
                        sourceFailure.compareAndSet(SourceFailure.NONE, SourceFailure.INVALID_READ)
                        throw IOException("Invalid video read")
                    }
                    if (size == 0) return 0
                    val result = nativeReadVideo(token, position, buffer, offset, size)
                    if (result < 0) {
                        val length = nativeVideoSize(token)
                        if (length < 0 || position < length) {
                            sourceFailure.compareAndSet(SourceFailure.NONE, SourceFailure.READ_UNAVAILABLE)
                            throw IOException("Video source unavailable")
                        }
                        if (sourceEof.compareAndSet(false, true))
                            Log.i("ChatterVideo", "Video source event=EOF_OBSERVED")
                    } else if (result > 0 && sourceRead.compareAndSet(false, true)) {
                        Log.i("ChatterVideo", "Video source event=READ_ACCEPTED")
                    }
                    return result
                }
                override fun close() {
                    if (sourceClosed.compareAndSet(false, true)) {
                        val event = if (released) "CLOSE_DURING_ACTIVITY_RELEASE" else "CLOSE_BEFORE_ACTIVITY_RELEASE"
                        Log.i("ChatterVideo", "Video source event=$event")
                    }
                    nativeReleaseVideo(token)
                }
            })
            // SDK contract: scaling requires a data source (Initialized), not Idle.
            stage = FailureStage.VIDEO_SCALING
            media.setVideoScalingMode(MediaPlayer.VIDEO_SCALING_MODE_SCALE_TO_FIT)
            stage = FailureStage.LISTENERS
            media.setOnVideoSizeChangedListener { _, width, height ->
                videoWidth = width; videoHeight = height; fitSurface()
            }
            media.setOnPreparedListener {
                if (!released) {
                    prepared = true
                    if (muted) media.setVolume(0f, 0f)
                    updatePlayback()
                    start()
                }
            }
            media.setOnCompletionListener { if (!released) updatePlayback() }
            media.setOnErrorListener { _, _, extra ->
                val failure = if (extra == MediaPlayer.MEDIA_ERROR_IO) {
                    if (prepared) FailureStage.PLAYER_IO_PLAYBACK else FailureStage.PLAYER_IO_PREPARE
                } else FailureStage.PLAYER_ERROR
                fail(failure)
                true
            }
            stage = FailureStage.PREPARE_ASYNC
            media.prepareAsync()
        } catch (_: Exception) { fail(stage) }
    }
    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
    override fun surfaceDestroyed(holder: SurfaceHolder) { release(); finish() }
    override fun onResume() { super.onResume(); foreground = true; clock.removeCallbacks(clockTick); clock.post(clockTick) }
    override fun onPause() { foreground = false; clock.removeCallbacks(clockTick); pause(); super.onPause() }
    override fun onStop() { super.onStop(); release(); finish() }
    override fun onDestroy() { clock.removeCallbacks(clockTick); release(); super.onDestroy() }

    private fun fitSurface() {
        if (!::frame.isInitialized || !::videoView.isInitialized || frame.width <= 0 || frame.height <= 0) return
        val widthAvailable = frame.width - frame.paddingLeft - frame.paddingRight
        val heightAvailable = frame.height - frame.paddingTop - frame.paddingBottom
        if (widthAvailable <= 0 || heightAvailable <= 0) return
        val aspect = if (videoWidth > 0 && videoHeight > 0) videoWidth.toDouble() / videoHeight else 16.0 / 9.0
        val mediaWidth = minOf(widthAvailable.toDouble(), heightAvailable * aspect).toInt().coerceAtLeast(1)
        val mediaHeight = (mediaWidth / aspect).toInt().coerceAtMost(heightAvailable).coerceAtLeast(1)
        val mediaX = (widthAvailable - mediaWidth) / 2
        val mediaY = (heightAvailable - mediaHeight) / 2
        if (videoView.layoutParams.width != mediaWidth || videoView.layoutParams.height != mediaHeight)
            videoView.layoutParams = FrameLayout.LayoutParams(mediaWidth, mediaHeight, Gravity.TOP or Gravity.START)
        videoView.translationX = mediaX.toFloat()
        videoView.translationY = mediaY.toFloat()
        val chromeWidth = minOf(widthAvailable, maxOf(dp(300), mediaWidth))
        val chromeX = (widthAvailable - chromeWidth) / 2f
        if (header.layoutParams.width != chromeWidth)
            header.layoutParams = FrameLayout.LayoutParams(chromeWidth, dp(48), Gravity.TOP or Gravity.START)
        if (footer.layoutParams.width != chromeWidth)
            footer.layoutParams = FrameLayout.LayoutParams(chromeWidth, dp(96), Gravity.TOP or Gravity.START)
        header.translationX = chromeX
        header.translationY = maxOf(0, mediaY - dp(48) - dp(8)).toFloat()
        footer.translationX = chromeX
        footer.translationY = minOf(heightAvailable - dp(96), mediaY + mediaHeight + dp(8)).coerceAtLeast(0).toFloat()
    }

    private fun formatTime(ms: Int): String {
        val seconds = ms.coerceAtLeast(0) / 1000
        return if (seconds >= 3600) "%d:%02d:%02d".format(seconds / 3600, seconds / 60 % 60, seconds % 60)
            else "%d:%02d".format(seconds / 60, seconds % 60)
    }
    private fun durationMs(): Int = if (prepared && !released) try { player?.duration ?: 0 } catch (_: IllegalStateException) { 0 } else 0
    private fun isPlaying(): Boolean = if (prepared && !released) try { player?.isPlaying == true } catch (_: IllegalStateException) { false } else false
    private fun updatePlayback() {
        if (!::seek.isInitialized || released) return
        val total = durationMs().coerceAtLeast(0)
        val current = if (prepared) try { player?.currentPosition ?: 0 } catch (_: IllegalStateException) { 0 } else 0
        elapsed.text = formatTime(current)
        duration.text = formatTime(total)
        if (!seeking) seek.progress = if (total > 0) (current.toLong() * 1000 / total).toInt() else 0
        val playing = isPlaying()
        playButton.setImageResource(if (playing) R.drawable.ic_video_pause else R.drawable.ic_video_play)
        playButton.contentDescription = if (playing) "Pause video" else "Play video"
        val packed = nativeViewerPosition(token)
        val index = packed ushr 32
        val count = packed and 0xffffffffL
        position.text = if (index > 0 && count > 0) "$index / $count" else ""
        val canPrevious = nativeViewerCanNavigate(token, -1)
        val canNext = nativeViewerCanNavigate(token, 1)
        previousButton.isEnabled = canPrevious
        previousButton.alpha = if (canPrevious) 1f else 0.38f
        nextButton.isEnabled = canNext
        nextButton.alpha = if (canNext) 1f else 0.38f
    }
    private fun togglePlayback() {
        if (!prepared || released) return
        if (isPlaying()) pause() else start()
        updatePlayback()
    }
    private fun toggleMute() {
        muted = !muted
        if (prepared && !released) player?.setVolume(if (muted) 0f else 1f, if (muted) 0f else 1f)
        muteButton.setImageResource(if (muted) R.drawable.ic_video_volume_off else R.drawable.ic_video_volume)
        muteButton.contentDescription = if (muted) "Unmute video" else "Mute video"
    }
    private fun seekTo(position: Int) {
        if (prepared && !released) {
            try { player?.seekTo(position.coerceIn(0, durationMs().coerceAtLeast(0))) }
            catch (_: IllegalStateException) { fail(FailureStage.SEEK) }
        }
    }
    private fun fail(stage: FailureStage) {
        if (released) return
        Log.e("ChatterVideo", "Playback failure stage=${stage.name} source_failure=${sourceFailure.get().name}" +
            " source_read=${sourceRead.get()} source_eof=${sourceEof.get()} source_closed=${sourceClosed.get()}")
        Toast.makeText(this, "Unable to play this video", Toast.LENGTH_LONG).show()
        release(); finish()
    }
    private fun release() {
        if (released) return
        released = true
        prepared = false
        clock.removeCallbacks(clockTick)
        try { player?.release() } catch (_: Exception) {
            // Cleanup must still revoke plaintext access if native release fails.
        } finally {
            player = null
            try { nativeReleaseVideo(token) } finally { abandonFocus() }
        }
    }
    private fun abandonFocus() {
        resumeOnFocusGain = false
        if (::audioManager.isInitialized && ::focus.isInitialized) {
            try { audioManager.abandonAudioFocusRequest(focus) } catch (_: Exception) {}
        }
        hasFocus = false
    }
    private fun pausePlayer() {
        if (prepared && !released) {
            try { if (player?.isPlaying == true) player?.pause() }
            catch (_: IllegalStateException) { fail(FailureStage.PAUSE) }
        }
        updatePlayback()
    }
    private fun start() {
        if (!prepared || released || !foreground) return
        if (!hasFocus) {
            try { hasFocus = audioManager.requestAudioFocus(focus) == AudioManager.AUDIOFOCUS_REQUEST_GRANTED }
            catch (_: Exception) { fail(FailureStage.AUDIO_FOCUS); return }
        }
        if (hasFocus) {
            try { player?.start() } catch (_: IllegalStateException) { fail(FailureStage.START) }
        }
        updatePlayback()
    }
    private fun pause() {
        resumeOnFocusGain = false
        pausePlayer()
        abandonFocus()
    }
}
