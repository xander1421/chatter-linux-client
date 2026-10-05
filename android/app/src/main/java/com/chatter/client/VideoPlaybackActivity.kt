package com.chatter.client

import android.app.Activity
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.media.MediaDataSource
import android.media.MediaPlayer
import android.os.Bundle
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.MediaController
import android.widget.Toast
import java.io.IOException

/** Private decoder surface. Plaintext is read from Rust memory/authenticated ranges,
 * never a URL, public file, temporary plaintext file, or an external player. */
class VideoPlaybackActivity : Activity(), SurfaceHolder.Callback, MediaController.MediaPlayerControl {
    private var token = 0L
    private var player: MediaPlayer? = null
    private var prepared = false
    private var released = false
    private var foreground = false
    private lateinit var controller: MediaController
    private lateinit var frame: FrameLayout
    private lateinit var videoView: SurfaceView
    private var videoWidth = 0
    private var videoHeight = 0
    private lateinit var audioManager: AudioManager
    private lateinit var focus: AudioFocusRequest
    private var hasFocus = false
    private var resumeOnFocusGain = false
    private external fun nativeVideoSize(token: Long): Long
    private external fun nativeReadVideo(token: Long, position: Long, buffer: ByteArray, offset: Int, count: Int): Int
    private external fun nativeReleaseVideo(token: Long)

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        token = intent.getLongExtra("video-token", 0)
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
        frame = FrameLayout(this)
        val surface = SurfaceView(this)
        videoView = surface
        frame.addView(surface, FrameLayout.LayoutParams(-1, -1))
        setContentView(frame)
        controller = MediaController(this)
        controller.setMediaPlayer(this)
        controller.setAnchorView(frame)
        surface.setOnClickListener { if (prepared) controller.show() }
        frame.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> fitSurface() }
        surface.holder.addCallback(this)
        if (token <= 0 || nativeVideoSize(token) <= 0) fail()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        if (released || player != null) return
        try {
            val media = MediaPlayer()
            player = media
            media.setAudioAttributes(AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
                .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE).build())
            media.setDisplay(holder)
            media.setVideoScalingMode(MediaPlayer.VIDEO_SCALING_MODE_SCALE_TO_FIT)
            media.setDataSource(object : MediaDataSource() {
                override fun getSize(): Long = nativeVideoSize(token)
                override fun readAt(position: Long, buffer: ByteArray, offset: Int, size: Int): Int {
                    if (position < 0 || offset < 0 || size < 0 || offset > buffer.size || size > buffer.size - offset) throw IOException("Invalid video read")
                    if (size == 0) return 0
                    val result = nativeReadVideo(token, position, buffer, offset, size)
                    if (result < 0) {
                        val length = nativeVideoSize(token)
                        if (length < 0 || position < length) throw IOException("Video source unavailable")
                    }
                    return result
                }
                override fun close() { nativeReleaseVideo(token) }
            })
            media.setOnVideoSizeChangedListener { _, width, height ->
                videoWidth = width; videoHeight = height; fitSurface()
            }
            media.setOnPreparedListener {
                if (!released) { prepared = true; start(); controller.show() }
            }
            media.setOnCompletionListener { if (!released) { pause(); controller.show(0) } }
            media.setOnErrorListener { _, _, _ -> fail(); true }
            media.prepareAsync()
        } catch (_: Exception) { fail() }
    }
    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
    override fun surfaceDestroyed(holder: SurfaceHolder) { release(); finish() }
    override fun onResume() { super.onResume(); foreground = true }
    override fun onPause() { foreground = false; pause(); super.onPause() }
    override fun onStop() { super.onStop(); release(); finish() }
    override fun onDestroy() { release(); super.onDestroy() }
    private fun fitSurface() {
        if (videoWidth <= 0 || videoHeight <= 0 || frame.width <= 0 || frame.height <= 0) return
        val scale = minOf(frame.width.toDouble() / videoWidth, frame.height.toDouble() / videoHeight)
        val width = (videoWidth * scale).toInt().coerceAtLeast(1)
        val height = (videoHeight * scale).toInt().coerceAtLeast(1)
        if (videoView.layoutParams.width != width || videoView.layoutParams.height != height)
            videoView.layoutParams = FrameLayout.LayoutParams(width, height, Gravity.CENTER)
    }
    private fun fail() {
        Toast.makeText(this, "Unable to play this video", Toast.LENGTH_LONG).show()
        release(); finish()
    }
    private fun release() {
        if (released) return
        released = true
        prepared = false
        if (::controller.isInitialized) {
            try { controller.hide() } catch (_: Exception) {}
        }
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
            catch (_: IllegalStateException) { fail() }
        }
    }
    override fun start() {
        if (!prepared || released || !foreground) return
        if (!hasFocus) {
            try { hasFocus = audioManager.requestAudioFocus(focus) == AudioManager.AUDIOFOCUS_REQUEST_GRANTED }
            catch (_: Exception) { fail(); return }
        }
        if (hasFocus) {
            try { player?.start() } catch (_: IllegalStateException) { fail() }
        }
    }
    override fun pause() {
        resumeOnFocusGain = false
        pausePlayer()
        abandonFocus()
    }
    override fun getDuration(): Int = if (prepared) player?.duration ?: 0 else 0
    override fun getCurrentPosition(): Int = if (prepared) player?.currentPosition ?: 0 else 0
    override fun seekTo(position: Int) {
        if (prepared && !released) {
            try { player?.seekTo(position.coerceIn(0, getDuration().coerceAtLeast(0))) }
            catch (_: IllegalStateException) { fail() }
        }
    }
    override fun isPlaying(): Boolean = prepared && player?.isPlaying == true
    override fun getBufferPercentage(): Int = if (prepared) 100 else 0
    override fun canPause() = true
    override fun canSeekBackward() = true
    override fun canSeekForward() = true
    override fun getAudioSessionId(): Int = player?.audioSessionId ?: 0
}
