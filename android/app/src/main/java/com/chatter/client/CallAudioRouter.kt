package com.chatter.client

import android.app.Activity
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
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
import org.json.JSONArray

/**
 * Applies the call audio route over AudioManager (Signal's SignalAudioManager
 * shape). The routing DECISION is Rust's (`chatter_core::calls::audio_route`):
 * this class reports which outputs exist ([nativeOnCommunicationDevices]) and
 * applies the route it is told to ([applyDevice]).
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
class CallAudioRouter(private val activity: Activity) {
    private val audioManager = activity.getSystemService(AudioManager::class.java)
    private val powerManager = activity.getSystemService(PowerManager::class.java)
    private val handler = Handler(Looper.getMainLooper())

    private var active = false
    private var savedMode = AudioManager.MODE_NORMAL
    private var focusRequest: AudioFocusRequest? = null
    private var proximityLock: PowerManager.WakeLock? = null

    private var legacyApplied: String? = null
    private var pendingKey: String? = null
    private var requestGeneration = 0
    private val sco = LegacyScoSession()
    private val scoReceiver = object : BroadcastReceiver() {
        @Suppress("DEPRECATION")
        override fun onReceive(context: Context?, intent: Intent?) {
            if (intent?.action == AudioManager.ACTION_SCO_AUDIO_STATE_UPDATED) {
                val state = intent.getIntExtra(AudioManager.EXTRA_SCO_AUDIO_STATE, -1)
                val previous = intent.getIntExtra(AudioManager.EXTRA_SCO_AUDIO_PREVIOUS_STATE, -1)
                Log.i(TAG, "SCO state=$state previous=$previous sticky=$isInitialStickyBroadcast requested=${sco.requested} generation=$requestGeneration")
                val disconnected = sco.update(scoState(state), scoState(previous), isInitialStickyBroadcast)
                if (sco.requested && sco.connected && !audioManager.isBluetoothScoOn) {
                    try {
                        audioManager.isBluetoothScoOn = true
                    } catch (_: SecurityException) {
                        legacyApplied?.let { failRequest(it, "SCO routing permission unavailable") }
                        return
                    }
                }
                val failed = legacyApplied?.takeIf { key -> communicationDevices().any {
                    deviceKey(it) == key && routeFor(it) == ROUTE_BLUETOOTH
                } }
                if (disconnected && failed != null) {
                    failRequest(failed, "SCO disconnected")
                } else reportDevices(false)
            }
        }
    }
    private val communicationListener = if (Build.VERSION.SDK_INT >= 31) {
        AudioManager.OnCommunicationDeviceChangedListener { reportDevices(false) }
    } else null

    private val deviceCallback = object : AudioDeviceCallback() {
        override fun onAudioDevicesAdded(addedDevices: Array<out AudioDeviceInfo>) = reportDevices(true)
        override fun onAudioDevicesRemoved(removedDevices: Array<out AudioDeviceInfo>) = reportDevices(true)
    }

    /** A call began. Registering the callback reports the current outputs. */
    fun start() {
        handler.post { startOnMain() }
    }

    private fun startOnMain() {
        if (active) {
            CallMediaService.start(activity)
            return
        }
        active = true
        CallMediaService.start(activity)
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
        if (Build.VERSION.SDK_INT >= 31) {
            communicationListener?.let { audioManager.addOnCommunicationDeviceChangedListener(activity.mainExecutor, it) }
        }
        if (Build.VERSION.SDK_INT < 31) {
            activity.registerReceiver(scoReceiver, IntentFilter(AudioManager.ACTION_SCO_AUDIO_STATE_UPDATED))
        }
        reportDevices(true)
        Log.i(TAG, "call audio started")
    }

    /** The dialling UI may have activated routing before permission admission. */
    fun retryMediaService() {
        handler.post {
            if (active && !activity.isDestroyed) CallMediaService.start(activity)
        }
    }

    /** The call ended: hand the audio system back as it was. */
    fun stop() {
        handler.post { stopOnMain() }
    }

    private fun stopOnMain() {
        if (!active) return
        active = false
        CallMediaService.stop(activity)
        audioManager.unregisterAudioDeviceCallback(deviceCallback)
        if (Build.VERSION.SDK_INT >= 31) {
            communicationListener?.let { audioManager.removeOnCommunicationDeviceChangedListener(it) }
        }
        if (Build.VERSION.SDK_INT < 31) activity.unregisterReceiver(scoReceiver)
        pendingKey = null
        requestGeneration += 1
        legacyApplied = null
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

    /** A stable OS device key, selected by core's communication policy. */
    fun applyDevice(key: String) {
        handler.post {
            if (!active) return@post
            val device = communicationDevices().firstOrNull { deviceKey(it) == key }
            if (device == null) {
                reportDevices(true)
                return@post
            }
            val route = routeFor(device)
            // Initial inventory and registerAudioDeviceCallback can both ask
            // for the same route. Do not add SCO client references or postpone
            // the original acknowledgement deadline on duplicate requests.
            val alreadyApplied = if (Build.VERSION.SDK_INT >= 31) {
                audioManager.communicationDevice?.let { deviceKey(it) } == key
            } else legacyApplied == key && (route != ROUTE_BLUETOOTH || sco.connected)
            if (pendingKey == key || alreadyApplied) {
                Log.i(TAG, "route request coalesced kind=$route generation=$requestGeneration")
                reportDevices(false)
                return@post
            }
            pendingKey = key
            val generation = ++requestGeneration
            Log.i(TAG, "route request kind=$route generation=$generation mode=${audioManager.mode}")
            var accepted = true
            try {
                if (Build.VERSION.SDK_INT >= 31) {
                    if (!audioManager.setCommunicationDevice(device)) {
                        accepted = false
                        Log.w(TAG, "Communication route refused by Android")
                    }
                } else {
                    legacyRoute(route)
                    legacyApplied = key
                }
            } catch (error: SecurityException) {
                accepted = false
                Log.w(TAG, "Communication routing permission unavailable")
            }
            reportDevices(false)
            if (!accepted) {
                failRequest(key, "request refused")
            } else if (pendingKey == key) {
                handler.postDelayed({
                    if (active && generation == requestGeneration && pendingKey == key) failRequest(key, "acknowledgement timeout")
                }, 30_000)
            }
        }
    }

    private fun failRequest(key: String, reason: String) {
        if (!active) return
        Log.w(TAG, "route failed reason=$reason generation=$requestGeneration sco.requested=${sco.requested} sco.connected=${sco.connected}")
        pendingKey = null
        requestGeneration += 1
        try {
            if (Build.VERSION.SDK_INT >= 31) audioManager.clearCommunicationDevice()
            else {
                legacyRoute(null)
                legacyApplied = null
            }
        } catch (_: SecurityException) {
            Log.w(TAG, "Cannot clear communication route")
        }
        reportDevices(false)
        nativeOnCommunicationDeviceFailed(key)
    }

    /** Android 8–11: speakerphone + SCO flags. `null` clears both. */
    @Suppress("DEPRECATION")
    private fun legacyRoute(route: Int?) {
        // Disable the speaker BEFORE starting SCO. Repeatedly writing false
        // after SCO has connected can clear FOR_COMMUNICATION on legacy hosts.
        if (route == ROUTE_BLUETOOTH) {
            if (audioManager.isSpeakerphoneOn) audioManager.isSpeakerphoneOn = false
            if (sco.begin()) {
                audioManager.startBluetoothSco()
                Log.i(TAG, "SCO start generation=$requestGeneration")
            }
            // Enable the routing flag on CONNECTED, never treat this setter as
            // an acknowledgement of a successful asynchronous SCO connection.
        } else {
            if (audioManager.isBluetoothScoOn) audioManager.isBluetoothScoOn = false
            if (sco.end()) {
                audioManager.stopBluetoothSco()
                Log.i(TAG, "SCO stop generation=$requestGeneration")
            }
            val speaker = route == ROUTE_SPEAKER
            if (audioManager.isSpeakerphoneOn != speaker) audioManager.isSpeakerphoneOn = speaker
        }
    }

    private fun scoState(state: Int): LegacyScoSession.State = when (state) {
        AudioManager.SCO_AUDIO_STATE_CONNECTED -> LegacyScoSession.State.CONNECTED
        AudioManager.SCO_AUDIO_STATE_CONNECTING -> LegacyScoSession.State.CONNECTING
        AudioManager.SCO_AUDIO_STATE_DISCONNECTED -> LegacyScoSession.State.DISCONNECTED
        else -> LegacyScoSession.State.UNKNOWN
    }

    private fun routeFor(device: AudioDeviceInfo): Int? = when {
        device.type in typesFor(ROUTE_EARPIECE) -> ROUTE_EARPIECE
        device.type in typesFor(ROUTE_SPEAKER) -> ROUTE_SPEAKER
        device.type in typesFor(ROUTE_WIRED) -> ROUTE_WIRED
        device.type in typesFor(ROUTE_BLUETOOTH) -> ROUTE_BLUETOOTH
        else -> null
    }

    private fun deviceKey(device: AudioDeviceInfo): String {
        val address = if (Build.VERSION.SDK_INT >= 28) device.address else ""
        return "${device.type}:$address:${device.productName}"
    }

    private fun communicationDevices(): List<AudioDeviceInfo> {
        val devices = if (Build.VERSION.SDK_INT >= 31) {
            audioManager.availableCommunicationDevices
        } else {
            audioManager.getDevices(AudioManager.GET_DEVICES_OUTPUTS).toList()
        }
        val supported = devices.filter {
            routeFor(it) != null && (Build.VERSION.SDK_INT >= 31 ||
                routeFor(it) != ROUTE_BLUETOOTH || it.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO)
        }
        // Pre-31 routing flags cannot choose between two devices of a class,
        // nor force the earpiece while a wired headset is plugged in.
        return if (Build.VERSION.SDK_INT >= 31) supported else {
            val wired = supported.any { routeFor(it) == ROUTE_WIRED }
            supported.filter { !wired || routeFor(it) != ROUTE_EARPIECE }.distinctBy { routeFor(it) }
        }
    }

    private fun reportDevices(changed: Boolean) {
        if (!active) return
        val devices = communicationDevices()
        val applied = if (Build.VERSION.SDK_INT >= 31) {
            audioManager.communicationDevice?.let { deviceKey(it) }
        } else {
            // Legacy SCO connection is asynchronous: don't claim Bluetooth
            // until AudioManager reports that SCO is on.
            legacyApplied?.takeIf { key -> devices.any {
                deviceKey(it) == key && (routeFor(it) != ROUTE_BLUETOOTH || sco.connected)
            } }
        }
        if (applied != null && applied == pendingKey) {
            Log.i(TAG, "route acknowledged generation=$requestGeneration sco.connected=${sco.connected}")
            pendingKey = null
        }
        val rows = JSONArray()
        devices.forEach { device ->
            val name = when (routeFor(device)) {
                ROUTE_EARPIECE -> "Phone earpiece"
                ROUTE_SPEAKER -> "Speaker"
                else -> device.productName.toString()
            }
            rows.put(JSONArray().put(deviceKey(device)).put(name).put(routeFor(device)))
        }
        setProximityLock(devices.any { deviceKey(it) == applied && routeFor(it) == ROUTE_EARPIECE })
        nativeOnCommunicationDevices(rows.toString(), applied ?: "")
        if (changed) nativeOnCommunicationDevicesChanged()
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
    private external fun nativeOnCommunicationDevices(snapshot: String, applied: String)
    private external fun nativeOnCommunicationDevicesChanged()
    private external fun nativeOnCommunicationDeviceFailed(key: String)

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
                AudioDeviceInfo.TYPE_USB_DEVICE,
                AudioDeviceInfo.TYPE_USB_ACCESSORY,
            )
            // TYPE_BLE_HEADSET (26) is an API 31 constant; the value is inlined,
            // and older releases simply never report it.
            ROUTE_BLUETOOTH -> setOf(
                AudioDeviceInfo.TYPE_BLUETOOTH_SCO,
                AudioDeviceInfo.TYPE_BLE_HEADSET,
                AudioDeviceInfo.TYPE_BLE_SPEAKER,
                AudioDeviceInfo.TYPE_HEARING_AID,
            )
            else -> emptySet()
        }
    }
}
