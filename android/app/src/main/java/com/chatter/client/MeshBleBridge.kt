package com.chatter.client

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattServer
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothStatusCodes
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.bluetooth.le.BluetoothLeAdvertiser
import android.bluetooth.le.BluetoothLeScanner
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelUuid
import android.os.SystemClock
import android.util.Log
import java.util.Locale
import java.util.UUID
import java.util.ArrayDeque
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong

/**
 * The Bluetooth LE radio for the mesh transport — both roles, simultaneously.
 *
 * Android's peripheral APIs (`BluetoothLeAdvertiser`, `BluetoothGattServer`) are
 * Java-only with no NDK equivalent, so the radio has to live here and Rust has to
 * reach it over JNI. The process is Rust-owned (Slint on GameActivity), which
 * inverts the usual arrangement: **Rust drives, Kotlin serves**.
 *
 * ## The contract
 *
 * The seven methods Rust calls — [start], [stop], [setScanning], [connect],
 * [disconnect], [send], [maxWrite] — and the five `native…` callbacks below are
 * mirrored in `chatter-mesh-android/src/lib.rs` (`BRIDGE_CLASS`, `BRIDGE_METHODS`,
 * `NATIVE_METHODS`) and asserted against this compiled class with `javap -s`.
 * A descriptor mismatch is the one failure mode that does not show up at compile
 * time, so **do not change a name, a parameter type or the package without
 * changing that Rust table in the same commit.**
 *
 * This is a Kotlin `object`, so every method above is an instance method on
 * `com.chatter.client.MeshBleBridge` and the singleton is reachable from JNI as
 * the static field `INSTANCE`:
 *
 * ```
 * FindClass("com/chatter/client/MeshBleBridge")
 *   → GetStaticFieldID("INSTANCE", "Lcom/chatter/client/MeshBleBridge;")
 *   → GetStaticObjectField → NewGlobalRef
 * ```
 *
 * The `native…` declarations are public on purpose: plain `javap -s` omits private
 * members, and the signature test has to be able to see them.
 *
 * ## Connected means writable
 *
 * [nativeOnConnected] is emitted only once the link can actually carry a write —
 * for a central link after service discovery *and* the notification-enabling CCCD
 * write completes, for a peripheral link once the remote central subscribes.
 * Emitting at `onConnectionStateChange` would produce a link that looks healthy
 * and drops every write.
 *
 * ## Threading
 *
 * Bluetooth callbacks arrive on binder threads, and a blocked binder callback
 * stalls the whole Bluetooth stack. So callbacks only touch concurrent/volatile
 * state, answer the stack, and hand everything else to the single `mesh-ble`
 * handler thread. Every native callback is dispatched from that one thread, which
 * also gives Rust a total order over events. Binder threads never take [lock].
 */
@SuppressLint("MissingPermission") // Every radio call is permission-checked in start() and SecurityException-guarded.
object MeshBleBridge {

    internal const val TAG = "MeshBleBridge"

    // --- Wire constants. Must match chatter-mesh-link::link. ---------------------

    /** bitchat mainnet service UUID. The debug build's ends `4B5A`; ours never does. */
    internal val SERVICE_UUID: UUID = UUID.fromString("F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5C")

    /** The single characteristic carrying all mesh traffic, both directions. */
    internal val CHARACTERISTIC_UUID: UUID = UUID.fromString("A1B2C3D4-E5F6-4A5B-8C9D-0E1F2A3B4C5D")

    /** Client Characteristic Configuration Descriptor — the standard 0x2902. */
    internal val CCCD_UUID: UUID = UUID.fromString("00002902-0000-1000-8000-00805F9B34FB")

    private val SERVICE_PARCEL_UUID = ParcelUuid(SERVICE_UUID)

    // --- Tuning ------------------------------------------------------------------

    /** Largest MTU the spec allows; the stack negotiates down from here. */
    internal const val REQUESTED_MTU = 517

    /** ATT opcode + handle. A write carries MTU minus this many bytes. */
    internal const val ATT_HEADER = 3

    /**
     * Frames held per peripheral link while it is connected but not yet writable.
     *
     * Small on purpose. The window it covers is one round trip — GATT connect to
     * CCCD subscribe — so a handful of frames is generous, and a peer that writes
     * without ever subscribing must not be able to grow this without bound.
     */
    internal const val MAX_PENDING_INBOUND = 8

    /**
     * How long a writing central may stay unsubscribed before we stop waiting for
     * its CCCD write and adopt the link anyway. See [adoptStaleSubscription].
     *
     * A fresh central subscribes well inside this (measured 0.6 s from connect,
     * bitchat-android 2.0.1 on an S25), so the normal handshake never reaches it.
     */
    internal const val SUBSCRIBE_GRACE_MS = 2_000L

    /**
     * How long a central link may take to become writable before we give up.
     * The stack's own connect timeout only covers the ACL; a peer that connects
     * and then never finishes discovery would otherwise leave Rust holding a
     * LinkId that never resolves either way.
     */
    private const val CONNECT_TIMEOUT_MS = 20_000L

    // --- State -------------------------------------------------------------------

    /** Guards the start/stop/advertise/scan lifecycle. Never taken on a binder thread. */
    internal val lock = Any()

    // `also { it.start() }`, not `apply { start() }`: inside apply the innermost
    // receiver would make a bare start() the thread's, one character away from
    // recursing into this object's own start().
    private val worker = HandlerThread("mesh-ble").also { it.start() }
    internal val handler = Handler(worker.looper)

    @Volatile private var appContext: Context? = null
    @Volatile internal var started = false

    /** Scanning as requested by the policy layer, which survives an adapter power cycle. */
    private var scanRequested = false

    /** Scanning as it actually is right now. */
    private var scanActive = false
    private var advertising = false
    // Leave one start of headroom below Android's five-in-30s scan throttle.
    // Keep timestamps across mesh off/on toggles within this process.
    private val scanStarts = ArrayDeque<Long>()
    private var scanDeferred = false
    private val scanResults = AtomicLong(0)
    private val lastScanResultAt = AtomicLong(0)
    private const val DISCOVERY_REFRESH_MS = 1_000L
    private data class RecentDiscovery(val peer: Long?, val sentAt: Long)
    private val recentDiscoveries = HashMap<String, RecentDiscovery>()
    private var silentRestartAt = 0L
    private var watchdogRestartPending = false
    private var silentWarningLogged = false
    private var radioUpAt = 0L
    private var lastSummaryAt = 0L
    private val scanWindowMs = 30_000L
    private val scanSilenceMs = 60_000L
    private val scanRetry = Runnable { synchronized(lock) { scanDeferred = false; applyScanLocked("deferred") } }
    private val radioHealth = object : Runnable {
        override fun run() {
            synchronized(lock) {
                if (!started) return
                val now = SystemClock.elapsedRealtime()
                val recentStarts = scanStarts.count { now - it < scanWindowMs }
                var recoveredScan = false
                if (now - lastSummaryAt >= scanSilenceMs) {
                    lastSummaryAt = now
                    Log.i(TAG, "radio summary: scanResults=${scanResults.get()} scanStarts30s=$recentStarts scanActive=$scanActive deferred=$scanDeferred centralLinks=${centralLinks.size} peripheralLinks=${peripheralLinks.size} advertising=$advertising")
                    // A failed asynchronous start can leave a scan request
                    // with neither a running scan nor a scheduled retry.
                    if (scanRequested && !scanActive && !scanDeferred) {
                        val adapter = context()?.let { adapterOf(it) }
                        val radioUp = adapter?.let { guard("isEnabled", false) { it.isEnabled } } == true
                        if (radioUp) {
                            Log.w(TAG, "scan requested but neither active nor deferred; recovering")
                            applyScanLocked("health recovery")
                            recoveredScan = true
                        }
                    }
                }
                val lastResult = lastScanResultAt.get().takeIf { it > 0 } ?: radioUpAt
                if (lastResult > silentRestartAt && silentRestartAt != 0L) {
                    silentRestartAt = 0L
                    silentWarningLogged = false
                }
                if (!recoveredScan && scanActive && !scanDeferred && now - lastResult >= scanSilenceMs) {
                    if (silentRestartAt == 0L) {
                        watchdogRestartPending = true
                        Log.w(TAG, "scanner silent for 60s; restarting once, scanResults=${scanResults.get()}")
                        stopScanLocked("watchdog")
                        applyScanLocked("watchdog")
                    } else if (!silentWarningLogged && now - silentRestartAt >= scanSilenceMs) {
                        silentWarningLogged = true
                        Log.w(TAG, "scanner still silent after restart: scanResults=${scanResults.get()} scanStarts30s=$recentStarts links=${centralLinks.size + peripheralLinks.size}")
                    }
                }
                handler.postDelayed(this, 5_000L)
            }
        }
    }
    internal var serviceReady = false

    /** Our mesh peer id, advertised in the scan response. Set before [start]. */
    private var localPeer: ByteArray = ByteArray(0)

    private var bluetoothManager: BluetoothManager? = null
    internal var gattServer: BluetoothGattServer? = null
    private var notifyCharacteristic: BluetoothGattCharacteristic? = null
    private var receiverRegistered = false

    internal val centralLinks = ConcurrentHashMap<String, CentralLink>()
    internal val peripheralLinks = ConcurrentHashMap<String, PeripheralLink>()

    init {
        // The library is normally already loaded by GameActivity (android.app.lib_name)
        // long before anything touches this object, but MeshService can be created
        // first on a cold restart. loadLibrary is idempotent.
        try {
            System.loadLibrary("chatter_ui")
        } catch (t: Throwable) {
            Log.e(TAG, "libchatter_ui not loaded; native callbacks will be dropped", t)
        }
    }

    /**
     * Link ids carry the role, because two devices can link in *both* directions at
     * once — we connect to them while they connect to us — and those are two
     * distinct links over the same address.
     */
    internal fun centralId(address: String) = "c:" + address.uppercase(Locale.ROOT)

    internal fun peripheralId(address: String) = "p:" + address.uppercase(Locale.ROOT)

    // --- Context ------------------------------------------------------------------

    /**
     * Hand this bridge an Android [Context]. Called by [MeshService] and callable
     * from Rust; without it the bridge falls back to reflection, since a
     * Rust-owned process has no other way to reach the Application object.
     */
    @Suppress("unused")
    fun attach(context: Context) {
        appContext = context.applicationContext
    }

    /**
     * The application context, or null if there genuinely is none.
     *
     * The reflective fallback is the only route from a static Kotlin scope to the
     * Application when nothing has called [attach] — both entry points are the
     * long-standing greylisted accessors, tried in order.
     */
    @SuppressLint("PrivateApi", "DiscouragedPrivateApi")
    private fun context(): Context? {
        appContext?.let { return it }
        val candidates = arrayOf(
            "android.app.ActivityThread" to "currentApplication",
            "android.app.AppGlobals" to "getInitialApplication",
        )
        for ((className, methodName) in candidates) {
            try {
                val found = Class.forName(className).getMethod(methodName).invoke(null) as? Context
                if (found != null) {
                    val app = found.applicationContext ?: found
                    appContext = app
                    return app
                }
            } catch (t: Throwable) {
                Log.w(TAG, "context(): $className.$methodName() unavailable", t)
            }
        }
        Log.e(TAG, "no application context; call attach() before start()")
        return null
    }

    private fun adapterOf(context: Context): BluetoothAdapter? {
        val manager = bluetoothManager
            ?: (context.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager)
                ?.also { bluetoothManager = it }
        return manager?.adapter
    }

    /**
     * The permissions this build actually needs, which differ by era.
     *
     * Below Android 12 scanning is counted as location, so `ACCESS_FINE_LOCATION`
     * is load-bearing: without it scans return nothing at all, which looks exactly
     * like "no peers nearby". From 12 on it is the three `BLUETOOTH_*` runtime
     * permissions instead. Both halves of the mesh are required — a node that can
     * only advertise or only scan drags down the whole mesh, not just itself.
     */
    private fun requiredPermissions(): Array<String> =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            arrayOf(
                Manifest.permission.BLUETOOTH_SCAN,
                Manifest.permission.BLUETOOTH_ADVERTISE,
                Manifest.permission.BLUETOOTH_CONNECT,
            )
        } else {
            @Suppress("DEPRECATION")
            arrayOf(
                Manifest.permission.BLUETOOTH,
                Manifest.permission.BLUETOOTH_ADMIN,
                Manifest.permission.ACCESS_FINE_LOCATION,
            )
        }

    private fun missingPermissions(context: Context): List<String> =
        requiredPermissions().filter {
            context.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED
        }

    internal inline fun <T> guard(what: String, fallback: T, block: () -> T): T =
        try {
            block()
        } catch (e: SecurityException) {
            // Android 12+ throws this from any Bluetooth call whose runtime
            // permission was revoked mid-session.
            Log.e(TAG, "$what: Bluetooth permission revoked", e)
            fallback
        } catch (e: IllegalStateException) {
            // The adapter can be torn down underneath a call in flight.
            Log.e(TAG, "$what: Bluetooth adapter unusable", e)
            fallback
        }

    // ==============================================================================
    // The seven methods Rust calls. Signatures are the contract — see BRIDGE_METHODS.
    // ==============================================================================

    /**
     * Begin advertising and accept inbound connections. `()Z`
     *
     * Returns false — rather than throwing — when the radio is unavailable: no
     * context, refused runtime permissions, no adapter, adapter off, or the GATT
     * server refusing to open. Advertising itself only reports failure
     * asynchronously via [AdvertiseCallback.onStartFailure], which is logged.
     */
    @Suppress("unused")
    fun start(): Boolean = synchronized(lock) {
        if (started) {
            if (serviceReady) beginAdvertisingLocked()
            applyScanLocked()
            return@synchronized true
        }

        val context = context() ?: return@synchronized false

        val missing = missingPermissions(context)
        if (missing.isNotEmpty()) {
            Log.w(TAG, "start: radio unavailable, permissions not granted: $missing")
            return@synchronized false
        }

        val adapter = adapterOf(context)
        if (adapter == null) {
            Log.w(TAG, "start: no Bluetooth adapter on this device")
            return@synchronized false
        }
        registerAdapterReceiverLocked(context)
        if (!guard("start/isEnabled", false) { adapter.isEnabled }) {
            // Keep the Rust runtime and receiver alive for STATE_ON recovery.
            started = true
            MeshService.start(context)
            emitAdapterPower(false)
            Log.w(TAG, "start: waiting for Bluetooth to be turned on")
            return@synchronized true
        }

        if (!openGattServerLocked(context)) {
            return@synchronized false
        }
        registerAdapterReceiverLocked(context)
        started = true
        radioUpAt = SystemClock.elapsedRealtime()
        lastSummaryAt = radioUpAt
        scanResults.set(0)
        lastScanResultAt.set(0)
        silentRestartAt = 0L
        watchdogRestartPending = false
        silentWarningLogged = false
        handler.removeCallbacks(radioHealth)
        handler.postDelayed(radioHealth, 5_000L)

        // Advertising begins from onServiceAdded: a central that connects before
        // the GATT database is published finds an empty service list.
        MeshService.start(context)
        Log.i(TAG, "started")
        true
    }

    /** Stop advertising, stop scanning, drop every link. `()V` */
    @Suppress("unused")
    fun stop() {
        synchronized(lock) { teardownLocked() }
        Log.i(TAG, "stopped")
    }

    /**
     * Turn scanning on or off without disturbing the peripheral role. `(Z)Z`
     *
     * This is the duty cycle's lever — scanning is the expensive half, so it runs
     * intermittently while advertising stays up. The request is remembered across
     * an adapter power cycle.
     */
    @Suppress("unused")
    fun setScanning(on: Boolean): Boolean = synchronized(lock) {
        if (!started) {
            Log.w(TAG, "setScanning($on): radio not started")
            return@synchronized false
        }
        scanRequested = on
        if (serviceReady) beginAdvertisingLocked()
        applyScanLocked(if (on) "policy on" else "policy off")
        // A deferred start is accepted and will run when the scan budget opens.
        scanActive == on || (on && scanDeferred)
    }

    /**
     * Publish our mesh peer id in the advertisement's scan response. `(Ljava/lang/String;)V`
     *
     * A BLE private address rotates, and nothing about it survives the rotation.
     * A scanner that cannot see who is behind an address has to connect to find
     * out — and that connection displaces the one it already had, whose loss
     * restarts this advertiser under yet another address. The id breaks that
     * loop. bitchat carries the same eight bytes in the same field for the same
     * reason (`BluetoothGattServerManager.kt`).
     *
     * Call before [start]; an advertiser already running will not pick it up.
     */
    @Suppress("unused")
    fun setLocalPeer(peerHex: String) = synchronized(lock) {
        localPeer = peerHex.hexToBytesOrEmpty()
        if (localPeer.isEmpty()) Log.w(TAG, "setLocalPeer: could not parse a peer id")
    }

    /**
     * Connect outward to a discovered address, taking the central role.
     * `(Ljava/lang/String;)Ljava/lang/String;`
     *
     * Returns the link id, or null if the connection could not even be attempted.
     * The id is returned before the link is writable; [nativeOnConnected] follows
     * once it is, and [nativeOnDisconnected] follows if it never gets there.
     * Calling this twice for the same address is idempotent.
     */
    @Suppress("unused")
    fun connect(address: String): String? {
        if (!started) {
            Log.w(TAG, "connect($address): radio not started")
            return null
        }
        val context = context() ?: return null
        val adapter = adapterOf(context) ?: return null
        val id = centralId(address)

        centralLinks[id]?.let {
            Log.d(TAG, "connect($address): link already exists")
            return id
        }

        val device = try {
            // Requires the canonical uppercase form; anything else throws.
            adapter.getRemoteDevice(address.uppercase(Locale.ROOT))
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "connect($address): not a Bluetooth address", e)
            return null
        }

        val link = CentralLink(id, device)
        centralLinks.putIfAbsent(id, link)?.let { return id }

        val gatt = guard("connect/connectGatt", null as BluetoothGatt?) {
            device.connectGatt(context, false, gattClientCallback, BluetoothDevice.TRANSPORT_LE)
        }
        if (gatt == null) {
            centralLinks.remove(id)
            Log.w(TAG, "connect($address): connectGatt refused")
            return null
        }
        link.gatt = gatt

        val watchdog = Runnable {
            if (!link.announced) failCentral(link, "did not become writable in time")
        }
        link.watchdog = watchdog
        handler.postDelayed(watchdog, CONNECT_TIMEOUT_MS)
        return id
    }

    /** Drop a link. `(Ljava/lang/String;)V` */
    @Suppress("unused")
    fun disconnect(link: String) {
        centralLinks.remove(link)?.let { central ->
            cancelWatchdog(central)
            closeCentral(central)
            emitDisconnected(link)
            return
        }
        peripheralLinks.remove(link)?.let { peripheral ->
            closePeripheral(peripheral)
            if (peripheral.announced) emitDisconnected(link)
            return
        }
        Log.d(TAG, "disconnect($link): unknown link")
    }

    /**
     * Write one payload to one link. `(Ljava/lang/String;[B)Z`
     *
     * False means "not sent, try again" — an unknown link, a link that is not
     * writable yet, an oversized payload, or a controller queue that is full. The
     * caller must not discard on false: nothing below retransmits.
     */
    @Suppress("unused")
    fun send(link: String, bytes: ByteArray): Boolean {
        centralLinks[link]?.let { return sendAsCentral(it, bytes) }
        peripheralLinks[link]?.let { return sendAsPeripheral(it, bytes) }
        Log.w(TAG, "send($link): unknown link")
        return false
    }

    /**
     * Largest single write this link accepts. `(Ljava/lang/String;)I`
     *
     * Zero means the link is gone, unknown, or not yet writable — the JNI boundary
     * has no null Int, so zero stands in for it. Otherwise this is the negotiated
     * ATT MTU minus 3, never an optimistic guess: a link that has not seen
     * `onMtuChanged` reports 20, because anything larger is silently dropped.
     */
    @Suppress("unused")
    fun maxWrite(link: String): Int {
        centralLinks[link]?.let { return if (it.announced) it.maxWrite else 0 }
        peripheralLinks[link]?.let { return if (it.announced) it.maxWrite else 0 }
        return 0
    }

    // ==============================================================================
    // Natives, implemented in Rust and registered from JNI_OnLoad. See NATIVE_METHODS.
    // Public so `javap -s` (which hides private members) can check the descriptors.
    // ==============================================================================

    /** `(Ljava/lang/String;I)V` — a device advertising our service came into range. */
    external fun nativeOnDiscovered(address: String, rssi: Int, peerHex: String?)

    /**
     * `(Ljava/lang/String;ZI)V` — a link is up **and writable right now**.
     *
     * `peripheral` is true when a remote central connected to us, false when we
     * connected outward as central. `maxWrite` is the negotiated ATT MTU minus 3.
     */
    external fun nativeOnConnected(link: String, peripheral: Boolean, maxWrite: Int)

    /** `(Ljava/lang/String;)V` — a link is gone; drop state keyed on it. */
    external fun nativeOnDisconnected(link: String)

    /** `(Ljava/lang/String;[B)V` — one write from the peer, not necessarily one packet. */
    external fun nativeOnData(link: String, bytes: ByteArray)

    /** `(Z)V` — the adapter was powered on or off underneath us. */
    external fun nativeOnAdapterPowerChanged(powered: Boolean)

    // --- Native dispatch ----------------------------------------------------------

    /**
     * All native callbacks leave from the one worker thread: binder threads stay
     * free, and Rust sees a single total order over events. Throwable rather than
     * Exception because an unregistered native throws UnsatisfiedLinkError, which
     * must not propagate into a Bluetooth callback.
     */
    private fun emit(what: String, block: () -> Unit) {
        handler.post {
            try {
                block()
            } catch (t: Throwable) {
                Log.e(TAG, "native callback $what failed", t)
            }
        }
    }

    private fun emitDiscovered(address: String, rssi: Int, peerHex: String?) =
        emit("onDiscovered") { nativeOnDiscovered(address, rssi, peerHex) }

    /** Preserve the first sighting and peer-id changes; refresh RSSI once a second. */
    private fun peerHex(peer: Long): String {
        val digits = "0123456789abcdef"
        return CharArray(16) { index ->
            digits[((peer ushr (60 - index * 4)) and 0xfL).toInt()]
        }.concatToString()
    }

    private fun emitScanResult(scan: ScanResult) {
        val address = scan.device.address
        val data = scan.scanRecord?.getServiceData(SERVICE_PARCEL_UUID)
        var peer: Long? = null
        if (data != null && data.size >= 8) {
            var value = 0L
            for (index in 0 until 8) value = (value shl 8) or (data[index].toLong() and 0xffL)
            peer = value
        }
        val now = SystemClock.elapsedRealtime()
        synchronized(recentDiscoveries) {
            val previous = recentDiscoveries[address]
            if (previous != null && previous.peer == peer && now - previous.sentAt < DISCOVERY_REFRESH_MS) return
            // Bound the cache even if a busy scan sees many rotating addresses.
            if (recentDiscoveries.size >= 256 && previous == null) recentDiscoveries.clear()
            recentDiscoveries[address] = RecentDiscovery(peer, now)
        }
        emitDiscovered(address, scan.rssi, peer?.let(::peerHex))
    }

    /** Sixteen hex characters to eight bytes; empty on anything else. */
    private fun String.hexToBytesOrEmpty(): ByteArray {
        if (length != 16) return ByteArray(0)
        return try {
            chunked(2).map { it.toInt(16).toByte() }.toByteArray()
        } catch (_: NumberFormatException) {
            ByteArray(0)
        }
    }

    internal fun emitConnected(link: String, peripheral: Boolean, maxWrite: Int) =
        emit("onConnected") { nativeOnConnected(link, peripheral, maxWrite) }

    internal fun emitDisconnected(link: String) =
        emit("onDisconnected") { nativeOnDisconnected(link) }

    internal fun emitData(link: String, bytes: ByteArray) =
        emit("onData") { nativeOnData(link, bytes) }

    private fun emitAdapterPower(powered: Boolean) =
        emit("onAdapterPowerChanged") { nativeOnAdapterPowerChanged(powered) }

    // --- Peripheral role: GATT server + advertiser ---------------------------------

    private fun openGattServerLocked(context: Context): Boolean {
        val manager = bluetoothManager ?: return false
        val server = guard("openGattServer", null as BluetoothGattServer?) {
            manager.openGattServer(context, gattServerCallback)
        }
        if (server == null) {
            Log.e(TAG, "openGattServer refused")
            return false
        }

        val characteristic = BluetoothGattCharacteristic(
            CHARACTERISTIC_UUID,
            // Write-without-response outbound, notify inbound: no ATT round trip
            // per packet in either direction.
            BluetoothGattCharacteristic.PROPERTY_WRITE_NO_RESPONSE or
                BluetoothGattCharacteristic.PROPERTY_NOTIFY,
            BluetoothGattCharacteristic.PERMISSION_WRITE,
        )
        characteristic.addDescriptor(
            BluetoothGattDescriptor(
                CCCD_UUID,
                BluetoothGattDescriptor.PERMISSION_READ or BluetoothGattDescriptor.PERMISSION_WRITE,
            ),
        )
        val service = BluetoothGattService(SERVICE_UUID, BluetoothGattService.SERVICE_TYPE_PRIMARY)
        service.addCharacteristic(characteristic)

        val added = guard("addService", false) { server.addService(service) }
        if (!added) {
            guard("closeGattServer", Unit) { server.close() }
            Log.i(TAG, "GATT server closed (addService refused)")
            Log.e(TAG, "addService refused")
            return false
        }

        gattServer = server
        Log.i(TAG, "GATT server opened")
        notifyCharacteristic = characteristic
        return true
    }

    internal fun beginAdvertisingLocked(): Boolean {
        if (advertising) return true
        if (!started || !serviceReady) return false
        val context = context() ?: return false
        val adapter = adapterOf(context) ?: return false
        val advertiser = guard("getBluetoothLeAdvertiser", null as BluetoothLeAdvertiser?) {
            adapter.bluetoothLeAdvertiser
        }
        if (advertiser == null) {
            Log.e(TAG, "this adapter cannot advertise; peripheral role unavailable")
            return false
        }

        val settings = AdvertiseSettings.Builder()
            // Balanced: continuous advertising is the always-on half of the mesh,
            // so the interval has to be affordable all day.
            .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_BALANCED)
            .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_MEDIUM)
            .setConnectable(true)
            .setTimeout(0)
            .build()

        val data = AdvertiseData.Builder()
            // The device name is a stable identifier broadcast to every passive
            // scanner in range. Never include it.
            .setIncludeDeviceName(false)
            .setIncludeTxPowerLevel(false)
            .addServiceUuid(SERVICE_PARCEL_UUID)
            .build()

        // The peer id rides in the scan response, not the advertisement: a
        // 128-bit service UUID plus eight bytes of service data does not fit in
        // one 31-byte packet, and the scan response is a second 31 bytes.
        //
        // Not a privacy regression. This id is already in the clear in every
        // announce we broadcast, and it names the mesh identity rather than the
        // hardware — unlike the device name, which stays excluded above.
        val scanResponse = if (localPeer.isEmpty()) null else AdvertiseData.Builder()
            .setIncludeDeviceName(false)
            .setIncludeTxPowerLevel(false)
            .addServiceData(SERVICE_PARCEL_UUID, localPeer)
            .build()

        Log.i(TAG, "advertising start requested")
        val issued = guard("startAdvertising", false) {
            advertiser.startAdvertising(settings, data, scanResponse, advertiseCallback)
            true
        }
        advertising = issued
        if (!issued) Log.e(TAG, "advertising start refused")
        return issued
    }

    private fun stopAdvertisingLocked() {
        if (!advertising) return
        advertising = false
        val context = context() ?: return
        val adapter = adapterOf(context) ?: return
        val advertiser = guard("getBluetoothLeAdvertiser", null as BluetoothLeAdvertiser?) {
            adapter.bluetoothLeAdvertiser
        } ?: return
        guard("stopAdvertising", Unit) { advertiser.stopAdvertising(advertiseCallback) }
        Log.i(TAG, "advertising stopped")
    }

    private val advertiseCallback = object : AdvertiseCallback() {
        override fun onStartSuccess(settingsInEffect: AdvertiseSettings?) {
            Log.i(TAG, "advertising start succeeded")
        }

        override fun onStartFailure(errorCode: Int) {
            // Asynchronous, so start() has already returned true. Loud, because a
            // node that only scans is invisible to peers scanning for it.
            Log.e(TAG, "advertising start failed: code=$errorCode (${advertiseError(errorCode)})")
            handler.post { synchronized(lock) { advertising = false } }
        }
    }

    private fun advertiseError(code: Int): String = when (code) {
        AdvertiseCallback.ADVERTISE_FAILED_ALREADY_STARTED -> "already started"
        AdvertiseCallback.ADVERTISE_FAILED_DATA_TOO_LARGE -> "advertise data too large"
        AdvertiseCallback.ADVERTISE_FAILED_FEATURE_UNSUPPORTED -> "unsupported on this device"
        AdvertiseCallback.ADVERTISE_FAILED_INTERNAL_ERROR -> "internal error"
        AdvertiseCallback.ADVERTISE_FAILED_TOO_MANY_ADVERTISERS -> "too many advertisers"
        else -> "code $code"
    }

    private val gattServerCallback = createGattServerCallback()

    // --- Central role: scanner + GATT client ---------------------------------------

    private fun stopScanLocked(reason: String) {
        if (!scanActive) return
        val context = context()
        val scanner = context?.let { ctx ->
            adapterOf(ctx)?.let { adapter ->
                guard("getBluetoothLeScanner", null as BluetoothLeScanner?) { adapter.bluetoothLeScanner }
            }
        }
        if (scanner != null) guard("stopScan", Unit) { scanner.stopScan(scanCallback) }
        scanActive = false
        synchronized(recentDiscoveries) { recentDiscoveries.clear() }
        Log.i(TAG, "scan stopped: $reason")
    }

    private fun applyScanLocked(reason: String = "policy") {
        if (!scanRequested) {
            handler.removeCallbacks(scanRetry)
            scanDeferred = false
            stopScanLocked(reason)
            return
        }
        if (!started || scanActive) return
        val context = context() ?: return
        val adapter = adapterOf(context) ?: return
        val enabled = guard("isEnabled", false) { adapter.isEnabled }
        val scanner = if (enabled) {
            guard("getBluetoothLeScanner", null as BluetoothLeScanner?) { adapter.bluetoothLeScanner }
        } else {
            null
        }
        if (scanner == null) {
            scanActive = false
            return
        }

        if (scanRequested && !scanActive) {
            val now = SystemClock.elapsedRealtime()
            while (scanStarts.isNotEmpty() && now - scanStarts.first() >= scanWindowMs) {
                scanStarts.removeFirst()
            }
            if (scanStarts.size >= 4) {
                val delay = scanWindowMs - (now - scanStarts.first()) + 1
                handler.removeCallbacks(scanRetry)
                // The policy re-asks every tick while deferred; log the first ask only.
                if (!scanDeferred) {
                    Log.i(TAG, "scan deferred: $reason, starts30s=${scanStarts.size}, delayMs=$delay")
                }
                scanDeferred = true
                handler.postDelayed(scanRetry, delay)
                return
            }
            handler.removeCallbacks(scanRetry)
            scanDeferred = false
            val filters = listOf(
                ScanFilter.Builder().setServiceUuid(SERVICE_PARCEL_UUID).build(),
            )
            val settings = ScanSettings.Builder()
                // The duty cycle is the caller's lever, so each window we do open
                // is scanned aggressively — a short low-power window finds nothing.
                .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
                .setCallbackType(ScanSettings.CALLBACK_TYPE_ALL_MATCHES)
                .setReportDelay(0)
                .build()
            synchronized(recentDiscoveries) { recentDiscoveries.clear() }
            scanStarts.addLast(now)
            scanActive = guard("startScan", false) {
                scanner.startScan(filters, settings, scanCallback)
                true
            }
            if (scanActive && watchdogRestartPending) {
                silentRestartAt = now
                watchdogRestartPending = false
            }
            Log.i(TAG, "scan start: $reason, active=$scanActive, starts30s=${scanStarts.size}")
        }
    }

    private val scanCallback = object : ScanCallback() {
        override fun onScanResult(callbackType: Int, result: ScanResult?) {
            val scan = result ?: return
            noteScanResult()
            emitScanResult(scan)
        }

        override fun onBatchScanResults(results: MutableList<ScanResult>?) {
            results?.forEach { noteScanResult(); emitScanResult(it) }
        }

        override fun onScanFailed(errorCode: Int) {
            Log.e(TAG, "scan failed: code=$errorCode (${scanError(errorCode)})")
            handler.post {
                synchronized(lock) {
                    // ALREADY_STARTED means the stack kept the previous scan, so
                    // our view is right and the request stands.
                    if (errorCode != SCAN_FAILED_ALREADY_STARTED) scanActive = false
                }
            }
        }
    }

    private fun noteScanResult() {
        scanResults.incrementAndGet()
        lastScanResultAt.set(SystemClock.elapsedRealtime())
    }

    private fun scanError(code: Int): String = when (code) {
        ScanCallback.SCAN_FAILED_ALREADY_STARTED -> "already started"
        ScanCallback.SCAN_FAILED_APPLICATION_REGISTRATION_FAILED -> "registration failed"
        ScanCallback.SCAN_FAILED_FEATURE_UNSUPPORTED -> "unsupported"
        ScanCallback.SCAN_FAILED_INTERNAL_ERROR -> "internal error"
        // Named in API 33, but the stack has reported both values for far longer.
        ScanCallback.SCAN_FAILED_OUT_OF_HARDWARE_RESOURCES -> "out of hardware resources"
        ScanCallback.SCAN_FAILED_SCANNING_TOO_FREQUENTLY -> "scanning too frequently"
        else -> "code $code"
    }

    private val gattClientCallback = createGattClientCallback()

    internal fun discoverOnce(link: CentralLink, gatt: BluetoothGatt) {
        if (!link.discoveryStarted.compareAndSet(false, true)) return
        val ok = guard("discoverServices", false) { gatt.discoverServices() }
        if (!ok) failCentral(link, "discoverServices refused")
    }

    @Suppress("DEPRECATION")
    internal fun writeCccd(gatt: BluetoothGatt, cccd: BluetoothGattDescriptor): Boolean {
        val value = BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
        return guard("writeDescriptor", false) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                gatt.writeDescriptor(cccd, value) == BluetoothStatusCodes.SUCCESS
            } else {
                cccd.setValue(value) && gatt.writeDescriptor(cccd)
            }
        }
    }

    internal fun cancelWatchdog(link: CentralLink) {
        link.watchdog?.let { handler.removeCallbacks(it) }
        link.watchdog = null
    }

    /** Tear a central link down and tell Rust, exactly once. */
    internal fun failCentral(link: CentralLink, reason: String) {
        if (centralLinks.remove(link.id) == null) return
        Log.w(TAG, "central link ${link.id}: $reason")
        cancelWatchdog(link)
        closeCentral(link)
        // Rust has held this id since connect() returned it, so it is told either
        // way — a link that never became writable still has to be reaped.
        emitDisconnected(link.id)
    }

    internal fun closeCentral(link: CentralLink) {
        if (!link.closed.compareAndSet(false, true)) return
        val gatt = link.gatt ?: return
        guard("disconnect", Unit) { gatt.disconnect() }
        // close() releases the client interface. There are few of them, and the
        // state that would need the disconnect callback is already gone.
        guard("close", Unit) { gatt.close() }
    }

    private fun closePeripheral(link: PeripheralLink) {
        // cancelConnection does not deliver onConnectionStateChange, which is why
        // the caller emits the event itself.
        val server = gattServer ?: return
        guard("cancelConnection", Unit) { server.cancelConnection(link.device) }
    }

    /**
     * Tell Rust a peripheral link is writable, then replay what it wrote while it
     * was not. Idempotent: the CCCD write and [adoptStaleSubscription] can race.
     */
    internal fun announcePeripheral(link: PeripheralLink, why: String) {
        synchronized(link) {
            if (link.announced || peripheralLinks[link.id] !== link) return
            link.announced = true
        }
        emitConnected(link.id, true, link.maxWrite)
        Log.i(TAG, "peripheral link ${link.id} writable ($why), maxWrite=${link.maxWrite}")
        // Anything they wrote while we could not yet name this link, in the
        // order they wrote it. Emitted after Connected so the layer above
        // already has the link when the data arrives.
        val held = synchronized(link.pendingInbound) {
            val frames = link.pendingInbound.toList()
            link.pendingInbound.clear()
            frames
        }
        if (held.isNotEmpty()) {
            Log.i(TAG, "flushing ${held.size} held frame(s) for ${link.id}")
            held.forEach { emitData(link.id, it) }
        }
    }

    /**
     * A central that keeps writing but never subscribes subscribed to a
     * *previous* incarnation of this GATT server.
     *
     * The connection belongs to the remote central, not to our process, so it
     * outlives a Chatter restart: the new server is told about it in
     * `onConnectionStateChange(STATE_CONNECTED)` straight after `addService`, but
     * the CCCD write that would announce it was made to the old process and is
     * never repeated. bitchat-android treats its client connection as healthy
     * and goes on writing its announces into it every few seconds — measured on
     * the S9 after a restart: `write from p:58:87:06:F2:2C:BD held until the
     * link is announced` every 30 s, indefinitely, with Rust never told the link
     * existed and so nothing ever notified back. Waiting for a CCCD write here
     * waits forever; with scanning unavailable it was the only link and the
     * mesh stayed at zero peers.
     *
     * The remote's notification registration lives in its own GATT client and
     * survived with the connection, so a notify reaches it. Its writes are the
     * evidence the link is live, and the largest of them bounds the MTU from
     * below (ours was negotiated, and reported, to the dead process).
     */
    internal fun adoptStaleSubscription(link: PeripheralLink) {
        link.adoptionScheduled.set(false)
        if (link.announced || peripheralLinks[link.id] !== link) return
        val pending = synchronized(link.pendingInbound) { link.pendingInbound.size }
        if (pending == 0) return
        val proven = link.largestInbound
        if (proven > link.maxWrite) link.maxWrite = proven
        Log.i(
            TAG,
            "peripheral link ${link.id} wrote for " +
                "${SystemClock.elapsedRealtime() - link.connectedAt} ms without subscribing: " +
                "adopting a subscription made before this process started",
        )
        announcePeripheral(link, "adopted")
    }

    // --- Sending -------------------------------------------------------------------

    private fun sendAsCentral(link: CentralLink, bytes: ByteArray): Boolean {
        if (!link.announced) {
            Log.w(TAG, "send(${link.id}): link not writable yet")
            return false
        }
        val gatt = link.gatt ?: return false
        val characteristic = link.characteristic ?: return false
        if (bytes.size > link.maxWrite) {
            // The controller drops oversized writes silently. Refusing here turns
            // that into a visible, retryable failure.
            Log.w(TAG, "send(${link.id}): ${bytes.size} bytes exceeds maxWrite ${link.maxWrite}")
            return false
        }
        if (!link.gate.tryAcquire()) return false

        val ok = writeCharacteristic(gatt, characteristic, bytes)
        if (!ok) link.gate.release()
        return ok
    }

    @Suppress("DEPRECATION")
    private fun writeCharacteristic(
        gatt: BluetoothGatt,
        characteristic: BluetoothGattCharacteristic,
        bytes: ByteArray,
    ): Boolean = guard("writeCharacteristic", false) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            gatt.writeCharacteristic(
                characteristic,
                bytes,
                BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE,
            ) == BluetoothStatusCodes.SUCCESS
        } else {
            // The pre-33 API carries the payload on the shared characteristic
            // object, so one link's write must not race another's.
            synchronized(characteristic) {
                characteristic.writeType = BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
                characteristic.setValue(bytes) && gatt.writeCharacteristic(characteristic)
            }
        }
    }

    private fun sendAsPeripheral(link: PeripheralLink, bytes: ByteArray): Boolean {
        if (!link.announced) {
            Log.w(TAG, "send(${link.id}): central has not subscribed")
            return false
        }
        val server = gattServer ?: return false
        val characteristic = notifyCharacteristic ?: return false
        if (bytes.size > link.maxWrite) {
            Log.w(TAG, "send(${link.id}): ${bytes.size} bytes exceeds maxWrite ${link.maxWrite}")
            return false
        }
        if (!link.gate.tryAcquire()) return false

        val ok = notifyLink(server, characteristic, link.device, bytes)
        if (!ok) link.gate.release()
        return ok
    }

    @Suppress("DEPRECATION")
    private fun notifyLink(
        server: BluetoothGattServer,
        characteristic: BluetoothGattCharacteristic,
        device: BluetoothDevice,
        bytes: ByteArray,
    ): Boolean = guard("notifyCharacteristicChanged", false) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            server.notifyCharacteristicChanged(device, characteristic, false, bytes) ==
                BluetoothStatusCodes.SUCCESS
        } else {
            // One characteristic instance is shared by every subscribed central on
            // the pre-33 API, so the value and the notify must be atomic together.
            synchronized(characteristic) {
                characteristic.setValue(bytes) &&
                    server.notifyCharacteristicChanged(device, characteristic, false)
            }
        }
    }

    // --- Adapter power --------------------------------------------------------------

    private val adapterStateReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action != BluetoothAdapter.ACTION_STATE_CHANGED) return
            when (intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)) {
                BluetoothAdapter.STATE_ON -> handler.post { onAdapterOn() }
                BluetoothAdapter.STATE_OFF -> handler.post { onAdapterOff() }
            }
        }
    }

    private fun registerAdapterReceiverLocked(context: Context) {
        if (receiverRegistered) return
        val filter = IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED)
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                context.registerReceiver(adapterStateReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
            } else {
                context.registerReceiver(adapterStateReceiver, filter)
            }
            receiverRegistered = true
        } catch (e: IllegalArgumentException) {
            Log.e(TAG, "could not observe adapter power state", e)
        }
    }

    private fun unregisterAdapterReceiverLocked(context: Context) {
        if (!receiverRegistered) return
        receiverRegistered = false
        try {
            context.unregisterReceiver(adapterStateReceiver)
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "adapter receiver was already gone", e)
        }
    }

    /** The adapter came back: rebuild the peripheral role and resume the duty cycle. */
    private fun onAdapterOn() = synchronized(lock) {
        if (started) {
            radioUpAt = SystemClock.elapsedRealtime()
            lastSummaryAt = radioUpAt
            scanResults.set(0)
            lastScanResultAt.set(0)
            silentRestartAt = 0L
            watchdogRestartPending = false
            silentWarningLogged = false
            handler.removeCallbacks(radioHealth)
            handler.postDelayed(radioHealth, 5_000L)
            val context = context()
            if (context != null && gattServer == null) {
                if (openGattServerLocked(context)) {
                    // Advertising resumes from onServiceAdded.
                    Log.i(TAG, "adapter on: GATT server reopened")
                } else {
                    Log.e(TAG, "adapter on: could not reopen the GATT server")
                }
            }
            applyScanLocked()
        }
        emitAdapterPower(true)
    }

    /** The adapter went away: every link died with it. */
    private fun onAdapterOff() = synchronized(lock) {
        handler.removeCallbacks(radioHealth)
        handler.removeCallbacks(scanRetry)
        scanDeferred = false
        watchdogRestartPending = false
        advertising = false
        if (scanActive) Log.i(TAG, "scan stopped: adapter off")
        scanActive = false
        dropAllLinks()
        gattServer?.let { server ->
            guard("closeGattServer", Unit) { server.close() }
            Log.i(TAG, "GATT server closed (adapter off)")
        }
        gattServer = null
        serviceReady = false
        notifyCharacteristic = null
        emitAdapterPower(false)
    }

    // --- Teardown ---------------------------------------------------------------------

    private fun dropAllLinks() {
        for (id in centralLinks.keys.toList()) {
            val link = centralLinks.remove(id) ?: continue
            cancelWatchdog(link)
            closeCentral(link)
            emitDisconnected(id)
        }
        for (id in peripheralLinks.keys.toList()) {
            val link = peripheralLinks.remove(id) ?: continue
            // Hang up the remote central too. Closing the GATT server alone
            // left it connected to a server that no longer exists: a Chatter
            // peer kept its subscription to the old service, our notifications
            // after a mesh off/on never reached it, and Noise to it could not
            // re-form (S25 -> S9, ui158).
            closePeripheral(link)
            if (link.announced) emitDisconnected(id)
        }
    }

    private fun teardownLocked() {
        handler.removeCallbacks(radioHealth)
        handler.removeCallbacks(scanRetry)
        scanDeferred = false
        watchdogRestartPending = false
        scanRequested = false
        applyScanLocked("radio stop")
        scanActive = false
        stopAdvertisingLocked()
        dropAllLinks()

        gattServer?.let { server ->
            guard("closeGattServer", Unit) { server.close() }
            Log.i(TAG, "GATT server closed (radio stop)")
        }
        gattServer = null
        serviceReady = false
        notifyCharacteristic = null

        val context = context()
        if (context != null) {
            unregisterAdapterReceiverLocked(context)
            MeshService.stop(context)
        }
        started = false
    }
}
