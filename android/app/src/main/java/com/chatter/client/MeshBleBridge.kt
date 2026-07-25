package com.chatter.client

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattServer
import android.bluetooth.BluetoothGattServerCallback
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
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
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean

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

    private const val TAG = "MeshBleBridge"

    // --- Wire constants. Must match chatter-mesh-link::link. ---------------------

    /** bitchat mainnet service UUID. The debug build's ends `4B5A`; ours never does. */
    private val SERVICE_UUID: UUID = UUID.fromString("F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5C")

    /** The single characteristic carrying all mesh traffic, both directions. */
    private val CHARACTERISTIC_UUID: UUID = UUID.fromString("A1B2C3D4-E5F6-4A5B-8C9D-0E1F2A3B4C5D")

    /** Client Characteristic Configuration Descriptor — the standard 0x2902. */
    private val CCCD_UUID: UUID = UUID.fromString("00002902-0000-1000-8000-00805F9B34FB")

    private val SERVICE_PARCEL_UUID = ParcelUuid(SERVICE_UUID)

    // --- Tuning ------------------------------------------------------------------

    /** Largest MTU the spec allows; the stack negotiates down from here. */
    private const val REQUESTED_MTU = 517

    /** ATT opcode + handle. A write carries MTU minus this many bytes. */
    private const val ATT_HEADER = 3

    /** Default ATT MTU before any negotiation, per the Bluetooth spec. */
    private const val DEFAULT_MTU = 23

    /** 23 − 3. What a link carries until `onMtuChanged` says otherwise. */
    private const val DEFAULT_MAX_WRITE = DEFAULT_MTU - ATT_HEADER

    /**
     * How long a central link may take to become writable before we give up.
     * The stack's own connect timeout only covers the ACL; a peer that connects
     * and then never finishes discovery would otherwise leave Rust holding a
     * LinkId that never resolves either way.
     */
    private const val CONNECT_TIMEOUT_MS = 20_000L

    /**
     * How long to wait for a write/notify completion callback before letting the
     * next one through. Only one ATT operation may be outstanding per link; if a
     * completion is ever lost, this stops the link wedging permanently.
     */
    private const val OP_TIMEOUT_MS = 2_000L

    // --- State -------------------------------------------------------------------

    /** Guards the start/stop/advertise/scan lifecycle. Never taken on a binder thread. */
    private val lock = Any()

    // `also { it.start() }`, not `apply { start() }`: inside apply the innermost
    // receiver would make a bare start() the thread's, one character away from
    // recursing into this object's own start().
    private val worker = HandlerThread("mesh-ble").also { it.start() }
    private val handler = Handler(worker.looper)

    @Volatile private var appContext: Context? = null
    @Volatile private var started = false

    /** Scanning as requested by the policy layer, which survives an adapter power cycle. */
    private var scanRequested = false

    /** Scanning as it actually is right now. */
    private var scanActive = false
    private var advertising = false

    private var bluetoothManager: BluetoothManager? = null
    private var gattServer: BluetoothGattServer? = null
    private var notifyCharacteristic: BluetoothGattCharacteristic? = null
    private var receiverRegistered = false

    private val centralLinks = ConcurrentHashMap<String, CentralLink>()
    private val peripheralLinks = ConcurrentHashMap<String, PeripheralLink>()

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

    // --- Link state ---------------------------------------------------------------

    /**
     * One ATT operation may be outstanding per link. The stack rejects the second
     * one, so we gate ourselves and report the rejection up as a retryable failure
     * rather than letting the packet vanish.
     */
    private class OpGate {
        private val busy = AtomicBoolean(false)
        @Volatile private var since = 0L

        fun tryAcquire(): Boolean {
            if (busy.compareAndSet(false, true)) {
                since = SystemClock.elapsedRealtime()
                return true
            }
            // A completion callback the stack never delivered would wedge this link
            // forever. Reclaim after the timeout: the worst case is one extra write
            // the controller rejects, which the layer above already handles.
            if (SystemClock.elapsedRealtime() - since > OP_TIMEOUT_MS) {
                since = SystemClock.elapsedRealtime()
                return true
            }
            return false
        }

        fun release() {
            busy.set(false)
        }
    }

    /** A link where we are the central: we connected outward to them. */
    private class CentralLink(val id: String, val device: BluetoothDevice) {
        @Volatile var gatt: BluetoothGatt? = null
        @Volatile var characteristic: BluetoothGattCharacteristic? = null
        @Volatile var maxWrite = DEFAULT_MAX_WRITE

        /** True once [nativeOnConnected] has been emitted, i.e. the link is writable. */
        @Volatile var announced = false

        /** Discovery is kicked off exactly once, whichever path gets there first. */
        val discoveryStarted = AtomicBoolean(false)
        val closed = AtomicBoolean(false)
        val gate = OpGate()

        @Volatile var watchdog: Runnable? = null
    }

    /** A link where we are the peripheral: a remote central connected inward. */
    private class PeripheralLink(val id: String, val device: BluetoothDevice) {
        @Volatile var maxWrite = DEFAULT_MAX_WRITE
        @Volatile var announced = false
        @Volatile var cccd: ByteArray = BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE
        val gate = OpGate()
    }

    /**
     * Link ids carry the role, because two devices can link in *both* directions at
     * once — we connect to them while they connect to us — and those are two
     * distinct links over the same address.
     */
    private fun centralId(address: String) = "c:" + address.uppercase(Locale.ROOT)

    private fun peripheralId(address: String) = "p:" + address.uppercase(Locale.ROOT)

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

    private inline fun <T> guard(what: String, fallback: T, block: () -> T): T =
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
        if (started) return@synchronized true

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
        if (!guard("start/isEnabled", false) { adapter.isEnabled }) {
            Log.w(TAG, "start: Bluetooth is off")
            return@synchronized false
        }

        if (!openGattServerLocked(context)) {
            return@synchronized false
        }
        registerAdapterReceiverLocked(context)
        started = true

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
        applyScanLocked()
        scanActive == on
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
    external fun nativeOnDiscovered(address: String, rssi: Int)

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

    private fun emitDiscovered(address: String, rssi: Int) =
        emit("onDiscovered") { nativeOnDiscovered(address, rssi) }

    private fun emitConnected(link: String, peripheral: Boolean, maxWrite: Int) =
        emit("onConnected") { nativeOnConnected(link, peripheral, maxWrite) }

    private fun emitDisconnected(link: String) =
        emit("onDisconnected") { nativeOnDisconnected(link) }

    private fun emitData(link: String, bytes: ByteArray) =
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
            Log.e(TAG, "addService refused")
            return false
        }

        gattServer = server
        notifyCharacteristic = characteristic
        return true
    }

    private fun beginAdvertisingLocked(): Boolean {
        if (advertising) return true
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

        val issued = guard("startAdvertising", false) {
            advertiser.startAdvertising(settings, data, advertiseCallback)
            true
        }
        advertising = issued
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
    }

    private val advertiseCallback = object : AdvertiseCallback() {
        override fun onStartSuccess(settingsInEffect: AdvertiseSettings?) {
            Log.i(TAG, "advertising")
        }

        override fun onStartFailure(errorCode: Int) {
            // Asynchronous, so start() has already returned true. Loud, because a
            // node that only scans is invisible to peers scanning for it.
            Log.e(TAG, "advertising failed: ${advertiseError(errorCode)}")
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

    private val gattServerCallback = object : BluetoothGattServerCallback() {

        override fun onServiceAdded(status: Int, service: BluetoothGattService?) {
            if (status != BluetoothGatt.GATT_SUCCESS) {
                Log.e(TAG, "onServiceAdded failed: status=$status")
                return
            }
            // Off the binder thread before taking the lifecycle lock.
            handler.post {
                synchronized(lock) { if (started) beginAdvertisingLocked() }
            }
        }

        override fun onConnectionStateChange(device: BluetoothDevice, status: Int, newState: Int) {
            val id = peripheralId(device.address)
            when (newState) {
                BluetoothProfile.STATE_CONNECTED -> {
                    // Connected, but not writable: the link is announced only once
                    // this central subscribes via its CCCD write.
                    peripheralLinks.computeIfAbsent(id) { PeripheralLink(id, device) }
                    Log.d(TAG, "peripheral link $id connected (awaiting subscription)")
                }

                BluetoothProfile.STATE_DISCONNECTED -> {
                    val removed = peripheralLinks.remove(id)
                    if (removed != null && removed.announced) emitDisconnected(id)
                }
            }
        }

        override fun onMtuChanged(device: BluetoothDevice, mtu: Int) {
            val link = peripheralLinks[peripheralId(device.address)] ?: return
            link.maxWrite = (mtu - ATT_HEADER).coerceAtLeast(DEFAULT_MAX_WRITE)
            Log.d(TAG, "peripheral link ${link.id} mtu=$mtu maxWrite=${link.maxWrite}")
        }

        override fun onCharacteristicWriteRequest(
            device: BluetoothDevice,
            requestId: Int,
            characteristic: BluetoothGattCharacteristic,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray?,
        ) {
            val id = peripheralId(device.address)
            val status = if (characteristic.uuid != CHARACTERISTIC_UUID) {
                BluetoothGatt.GATT_FAILURE
            } else if (preparedWrite) {
                // We publish write-without-response only; queued long writes are
                // out of contract and must be refused rather than half-handled.
                BluetoothGatt.GATT_REQUEST_NOT_SUPPORTED
            } else if (offset != 0) {
                BluetoothGatt.GATT_INVALID_OFFSET
            } else {
                val link = peripheralLinks[id]
                if (value != null && value.isNotEmpty()) {
                    if (link != null && link.announced) {
                        // Copy: the stack owns and reuses this buffer.
                        emitData(id, value.copyOf())
                    } else {
                        // Rust has no id for this link — it either never subscribed
                        // or was already dropped — so there is nowhere to deliver.
                        Log.d(TAG, "write from $id on an unannounced link, dropped")
                    }
                }
                BluetoothGatt.GATT_SUCCESS
            }
            if (responseNeeded) {
                guard("sendResponse", false) {
                    gattServer?.sendResponse(device, requestId, status, offset, null) ?: false
                }
            }
        }

        override fun onCharacteristicReadRequest(
            device: BluetoothDevice,
            requestId: Int,
            offset: Int,
            characteristic: BluetoothGattCharacteristic,
        ) {
            // The characteristic carries no readable state — traffic arrives by
            // notification. Refuse explicitly so the peer fails fast.
            guard("sendResponse", false) {
                gattServer?.sendResponse(
                    device,
                    requestId,
                    BluetoothGatt.GATT_READ_NOT_PERMITTED,
                    offset,
                    null,
                ) ?: false
            }
        }

        override fun onDescriptorWriteRequest(
            device: BluetoothDevice,
            requestId: Int,
            descriptor: BluetoothGattDescriptor,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray?,
        ) {
            var status = BluetoothGatt.GATT_SUCCESS
            if (descriptor.uuid == CCCD_UUID && value != null) {
                val id = peripheralId(device.address)
                val link = peripheralLinks.computeIfAbsent(id) { PeripheralLink(id, device) }
                link.cccd = value.copyOf()
                val subscribed =
                    value.contentEquals(BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE) ||
                        value.contentEquals(BluetoothGattDescriptor.ENABLE_INDICATION_VALUE)
                if (subscribed && !link.announced) {
                    // This is the moment a peripheral link becomes writable.
                    link.announced = true
                    emitConnected(id, true, link.maxWrite)
                    Log.i(TAG, "peripheral link $id writable, maxWrite=${link.maxWrite}")
                } else if (!subscribed && link.announced) {
                    // Unsubscribed: still connected, no longer writable, which the
                    // layer above only understands as gone.
                    link.announced = false
                    emitDisconnected(id)
                }
            } else if (descriptor.uuid != CCCD_UUID) {
                status = BluetoothGatt.GATT_FAILURE
            }
            if (responseNeeded) {
                guard("sendResponse", false) {
                    gattServer?.sendResponse(device, requestId, status, offset, value) ?: false
                }
            }
        }

        override fun onDescriptorReadRequest(
            device: BluetoothDevice,
            requestId: Int,
            offset: Int,
            descriptor: BluetoothGattDescriptor,
        ) {
            val link = peripheralLinks[peripheralId(device.address)]
            val isCccd = descriptor.uuid == CCCD_UUID
            val status =
                if (isCccd) BluetoothGatt.GATT_SUCCESS else BluetoothGatt.GATT_READ_NOT_PERMITTED
            val value = if (isCccd) {
                link?.cccd ?: BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE
            } else {
                null
            }
            guard("sendResponse", false) {
                gattServer?.sendResponse(device, requestId, status, offset, value) ?: false
            }
        }

        override fun onNotificationSent(device: BluetoothDevice, status: Int) {
            peripheralLinks[peripheralId(device.address)]?.gate?.release()
            if (status != BluetoothGatt.GATT_SUCCESS) {
                Log.w(TAG, "notify to ${device.address} failed: status=$status")
            }
        }

        override fun onExecuteWrite(device: BluetoothDevice, requestId: Int, execute: Boolean) {
            // We refuse prepared writes above, so there is never a queue to commit.
            // Answer anyway: an unanswered request stalls the peer's ATT bearer.
            guard("sendResponse", false) {
                gattServer?.sendResponse(
                    device,
                    requestId,
                    BluetoothGatt.GATT_REQUEST_NOT_SUPPORTED,
                    0,
                    null,
                ) ?: false
            }
        }
    }

    // --- Central role: scanner + GATT client ---------------------------------------

    private fun applyScanLocked() {
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
            scanActive = guard("startScan", false) {
                scanner.startScan(filters, settings, scanCallback)
                true
            }
        } else if (!scanRequested && scanActive) {
            guard("stopScan", Unit) { scanner.stopScan(scanCallback) }
            scanActive = false
        }
    }

    private val scanCallback = object : ScanCallback() {
        override fun onScanResult(callbackType: Int, result: ScanResult?) {
            val scan = result ?: return
            emitDiscovered(scan.device.address, scan.rssi)
        }

        override fun onBatchScanResults(results: MutableList<ScanResult>?) {
            results?.forEach { emitDiscovered(it.device.address, it.rssi) }
        }

        override fun onScanFailed(errorCode: Int) {
            Log.e(TAG, "scan failed: ${scanError(errorCode)}")
            handler.post {
                synchronized(lock) {
                    // ALREADY_STARTED means the stack kept the previous scan, so
                    // our view is right and the request stands.
                    if (errorCode != SCAN_FAILED_ALREADY_STARTED) scanActive = false
                }
            }
        }
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

    private val gattClientCallback = object : BluetoothGattCallback() {

        override fun onConnectionStateChange(gatt: BluetoothGatt, status: Int, newState: Int) {
            val id = centralId(gatt.device.address)
            if (newState == BluetoothProfile.STATE_CONNECTED &&
                status == BluetoothGatt.GATT_SUCCESS
            ) {
                val link = centralLinks[id]
                if (link == null) {
                    // We already gave up on this one; do not leave a client
                    // interface allocated, there are only so many.
                    guard("close", Unit) { gatt.close() }
                    return
                }
                // MTU first, then discovery, then subscribe: max_write must be
                // correct before the link is announced, because fragment sizing
                // upstream is derived from it.
                val requested = guard("requestMtu", false) { gatt.requestMtu(REQUESTED_MTU) }
                if (!requested) {
                    Log.w(TAG, "$id: requestMtu refused; continuing at $DEFAULT_MAX_WRITE bytes")
                    discoverOnce(link, gatt)
                }
            } else if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                // Removal is the guard: whoever removes the link emits the event,
                // so a teardown started here and one started by disconnect() cannot
                // both report it.
                val removed = centralLinks.remove(id)
                if (removed != null) {
                    cancelWatchdog(removed)
                    closeCentral(removed)
                    emitDisconnected(id)
                } else {
                    guard("close", Unit) { gatt.close() }
                }
                if (status != BluetoothGatt.GATT_SUCCESS) {
                    Log.d(TAG, "$id: disconnected with status=$status")
                }
            }
        }

        override fun onMtuChanged(gatt: BluetoothGatt, mtu: Int, status: Int) {
            val link = centralLinks[centralId(gatt.device.address)] ?: return
            if (status == BluetoothGatt.GATT_SUCCESS) {
                link.maxWrite = (mtu - ATT_HEADER).coerceAtLeast(DEFAULT_MAX_WRITE)
            } else {
                Log.w(TAG, "${link.id}: MTU negotiation failed, staying at $DEFAULT_MAX_WRITE")
            }
            // The peer may renegotiate later; discovery still runs exactly once.
            discoverOnce(link, gatt)
        }

        override fun onServicesDiscovered(gatt: BluetoothGatt, status: Int) {
            val link = centralLinks[centralId(gatt.device.address)] ?: return
            if (status != BluetoothGatt.GATT_SUCCESS) {
                failCentral(link, "service discovery failed: status=$status")
                return
            }
            val service = gatt.getService(SERVICE_UUID)
            if (service == null) {
                failCentral(link, "peer does not expose the mesh service")
                return
            }
            val characteristic = service.getCharacteristic(CHARACTERISTIC_UUID)
            if (characteristic == null) {
                failCentral(link, "peer does not expose the mesh characteristic")
                return
            }
            characteristic.writeType = BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
            link.characteristic = characteristic

            val enabled = guard("setCharacteristicNotification", false) {
                gatt.setCharacteristicNotification(characteristic, true)
            }
            if (!enabled) {
                failCentral(link, "could not enable notifications locally")
                return
            }
            val cccd = characteristic.getDescriptor(CCCD_UUID)
            if (cccd == null) {
                failCentral(link, "characteristic has no CCCD")
                return
            }
            // Local notification routing is not enough — the peer only starts
            // sending once this descriptor write lands, so the link is not yet
            // writable in either direction.
            if (!writeCccd(gatt, cccd)) {
                failCentral(link, "CCCD write refused")
            }
        }

        override fun onDescriptorWrite(
            gatt: BluetoothGatt,
            descriptor: BluetoothGattDescriptor,
            status: Int,
        ) {
            if (descriptor.uuid != CCCD_UUID) return
            val link = centralLinks[centralId(gatt.device.address)] ?: return
            if (status != BluetoothGatt.GATT_SUCCESS) {
                failCentral(link, "CCCD write failed: status=$status")
                return
            }
            if (link.announced) return
            // Discovered, subscribed, MTU known: the link can carry a write now.
            link.announced = true
            cancelWatchdog(link)
            emitConnected(link.id, false, link.maxWrite)
            Log.i(TAG, "central link ${link.id} writable, maxWrite=${link.maxWrite}")
        }

        // Pre-33 stacks only ever call this overload; 33+ never do, because the
        // three-argument one below overrides their delegation.
        @Suppress("DEPRECATION", "OVERRIDE_DEPRECATION")
        override fun onCharacteristicChanged(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
        ) {
            if (characteristic.uuid != CHARACTERISTIC_UUID) return
            val value = characteristic.value ?: return
            emitData(centralId(gatt.device.address), value.copyOf())
        }

        // API 33+ delivers the value as a parameter and calls only this overload,
        // so both exist and neither delegates: exactly one delivery per notification.
        override fun onCharacteristicChanged(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray,
        ) {
            if (characteristic.uuid != CHARACTERISTIC_UUID) return
            emitData(centralId(gatt.device.address), value.copyOf())
        }

        override fun onCharacteristicWrite(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            status: Int,
        ) {
            val link = centralLinks[centralId(gatt.device.address)] ?: return
            link.gate.release()
            if (status != BluetoothGatt.GATT_SUCCESS) {
                Log.w(TAG, "${link.id}: write failed status=$status")
            }
        }
    }

    private fun discoverOnce(link: CentralLink, gatt: BluetoothGatt) {
        if (!link.discoveryStarted.compareAndSet(false, true)) return
        val ok = guard("discoverServices", false) { gatt.discoverServices() }
        if (!ok) failCentral(link, "discoverServices refused")
    }

    @Suppress("DEPRECATION")
    private fun writeCccd(gatt: BluetoothGatt, cccd: BluetoothGattDescriptor): Boolean {
        val value = BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
        return guard("writeDescriptor", false) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                gatt.writeDescriptor(cccd, value) == BluetoothStatusCodes.SUCCESS
            } else {
                cccd.setValue(value) && gatt.writeDescriptor(cccd)
            }
        }
    }

    private fun cancelWatchdog(link: CentralLink) {
        link.watchdog?.let { handler.removeCallbacks(it) }
        link.watchdog = null
    }

    /** Tear a central link down and tell Rust, exactly once. */
    private fun failCentral(link: CentralLink, reason: String) {
        if (centralLinks.remove(link.id) == null) return
        Log.w(TAG, "central link ${link.id}: $reason")
        cancelWatchdog(link)
        closeCentral(link)
        // Rust has held this id since connect() returned it, so it is told either
        // way — a link that never became writable still has to be reaped.
        emitDisconnected(link.id)
    }

    private fun closeCentral(link: CentralLink) {
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
        advertising = false
        scanActive = false
        dropAllLinks()
        gattServer?.let { server -> guard("closeGattServer", Unit) { server.close() } }
        gattServer = null
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
            if (link.announced) emitDisconnected(id)
        }
    }

    private fun teardownLocked() {
        scanRequested = false
        applyScanLocked()
        scanActive = false
        stopAdvertisingLocked()
        dropAllLinks()

        gattServer?.let { server ->
            guard("closeGattServer", Unit) { server.close() }
        }
        gattServer = null
        notifyCharacteristic = null

        val context = context()
        if (context != null) {
            unregisterAdapterReceiverLocked(context)
            MeshService.stop(context)
        }
        started = false
    }
}
