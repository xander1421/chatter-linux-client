package com.chatter.client

import android.app.Activity
import android.media.AudioAttributes
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.util.Log

/**
 * Applies the call audio route over AudioManager (Signal's SignalAudioManager
 * shape). The routing DECISION is Rust's (`chatter_core::calls::audio_route`):
 * this class reports which outputs exist ([nativeOnAudioOutputsChanged]) and
 * applies the route it is told to ([apply]).
 *
 * For the call's duration: MODE_IN_COMMUNICATION, transient audio focus, an
 * AudioDeviceCallback for headsets coming and going, and — while the route is
 * the earpiece — the proximity wake lock, so the screen goes dark against the
 * ear. The playback stream is USAGE_VOICE_COMMUNICATION (audio_playback.rs),
 * which is the usage that follows the communication device.
 *
 * Everything runs on the main looper, so calls arriving from Rust's threads are
 * serialized with the device callbacks.
 */
class CallAudioRouter(activity: Activity) {
    private val audioManager = activity.getSystemService(AudioManager::class.java)
    private val powerManager = activity.getSystemService(PowerManager::class.java)
    private val handler = Handler(Looper.getMainLooper())

    private var active = false
    private var savedMode = AudioManager.MODE_NORMAL
    private var focusRequest: AudioFocusRequest? = null
    private var proximityLock: PowerManager.WakeLock? = null

    private val deviceCallback = object : AudioDeviceCallback() {
        override fun onAudioDevicesAdded(addedDevices: Array<out AudioDeviceInfo>) = reportOutputs()
        override fun onAudioDevicesRemoved(removedDevices: Array<out AudioDeviceInfo>) = reportOutputs()
    }

    /** A call began. Registering the callback reports the current outputs. */
    fun start() {
        handler.post { startOnMain() }
    }

    private fun startOnMain() {
        if (active) return
        active = true
        savedMode = audioManager.mode
        val attrs = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
            .build()
        focusRequest = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT)
            .setAudioAttributes(attrs)
            .build()
            .also { audioManager.requestAudioFocus(it) }
        audioManager.mode = AudioManager.MODE_IN_COMMUNICATION
        audioManager.registerAudioDeviceCallback(deviceCallback, handler)
        Log.i(TAG, "call audio started")
    }

    /** The call ended: hand the audio system back as it was. */
    fun stop() {
        handler.post { stopOnMain() }
    }

    private fun stopOnMain() {
        if (!active) return
        active = false
        audioManager.unregisterAudioDeviceCallback(deviceCallback)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            audioManager.clearCommunicationDevice()
        } else {
            legacyRoute(null)
        }
        audioManager.mode = savedMode
        focusRequest?.let { audioManager.abandonAudioFocusRequest(it) }
        focusRequest = null
        setProximityLock(false)
        Log.i(TAG, "call audio stopped")
    }

    /** Route values match `AudioRoute::as_i32` in Rust. */
    fun apply(route: Int) {
        handler.post { applyOnMain(route) }
    }

    private fun applyOnMain(route: Int) {
        if (!active) return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            val types = typesFor(route)
            val device = audioManager.availableCommunicationDevices.firstOrNull { it.type in types }
            if (device == null || !audioManager.setCommunicationDevice(device)) {
                Log.w(TAG, "route $route: no usable communication device")
            }
        } else {
            legacyRoute(route)
        }
        setProximityLock(route == ROUTE_EARPIECE)
        Log.i(TAG, "call audio route -> $route")
    }

    /** Android 8–11: speakerphone + SCO flags. `null` clears both. */
    @Suppress("DEPRECATION")
    private fun legacyRoute(route: Int?) {
        if (route == ROUTE_BLUETOOTH) {
            audioManager.startBluetoothSco()
            audioManager.isBluetoothScoOn = true
        } else if (audioManager.isBluetoothScoOn) {
            audioManager.isBluetoothScoOn = false
            audioManager.stopBluetoothSco()
        }
        audioManager.isSpeakerphoneOn = route == ROUTE_SPEAKER
    }

    private fun reportOutputs() {
        if (!active) return
        val devices = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            audioManager.availableCommunicationDevices
        } else {
            audioManager.getDevices(AudioManager.GET_DEVICES_OUTPUTS).toList()
        }
        val types = devices.map { it.type }.toSet()
        nativeOnAudioOutputsChanged(
            types.contains(AudioDeviceInfo.TYPE_BUILTIN_EARPIECE),
            types.any { it in typesFor(ROUTE_WIRED) },
            types.any { it in typesFor(ROUTE_BLUETOOTH) },
        )
    }

    private fun setProximityLock(on: Boolean) {
        if (on) {
            if (proximityLock == null &&
                powerManager.isWakeLockLevelSupported(PowerManager.PROXIMITY_SCREEN_OFF_WAKE_LOCK)
            ) {
                proximityLock = powerManager
                    .newWakeLock(PowerManager.PROXIMITY_SCREEN_OFF_WAKE_LOCK, "chatter:call-proximity")
                    .also { it.acquire() }
            }
        } else {
            proximityLock?.let {
                if (it.isHeld) it.release(PowerManager.RELEASE_FLAG_WAIT_FOR_NO_PROXIMITY)
            }
            proximityLock = null
        }
    }

    /** Implemented in Rust (platform/android/audio_route.rs). */
    private external fun nativeOnAudioOutputsChanged(earpiece: Boolean, wired: Boolean, bluetooth: Boolean)

    private companion object {
        const val TAG = "ChatterCallAudio"
        const val ROUTE_EARPIECE = 0
        const val ROUTE_SPEAKER = 1
        const val ROUTE_WIRED = 2
        const val ROUTE_BLUETOOTH = 3

        fun typesFor(route: Int): Set<Int> = when (route) {
            ROUTE_EARPIECE -> setOf(AudioDeviceInfo.TYPE_BUILTIN_EARPIECE)
            ROUTE_SPEAKER -> setOf(AudioDeviceInfo.TYPE_BUILTIN_SPEAKER)
            ROUTE_WIRED -> setOf(
                AudioDeviceInfo.TYPE_WIRED_HEADSET,
                AudioDeviceInfo.TYPE_WIRED_HEADPHONES,
                AudioDeviceInfo.TYPE_USB_HEADSET,
            )
            // TYPE_BLE_HEADSET (26) is an API 31 constant; the value is inlined,
            // and older releases simply never report it.
            ROUTE_BLUETOOTH -> setOf(AudioDeviceInfo.TYPE_BLUETOOTH_SCO, AudioDeviceInfo.TYPE_BLE_HEADSET)
            else -> emptySet()
        }
    }
}
