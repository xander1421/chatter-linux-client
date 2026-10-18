package com.chatter.client

import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.os.SystemClock
import java.util.concurrent.atomic.AtomicBoolean

/** ATT link bookkeeping shared by the central and peripheral roles. */
/**
 * How long to wait for a write/notify completion callback before letting the
 * next one through. Only one ATT operation may be outstanding per link; if a
 * completion is ever lost, this stops the link wedging permanently.
 */
private const val OP_TIMEOUT_MS = 2_000L
/** Default ATT MTU before any negotiation, per the Bluetooth spec. */
private const val DEFAULT_MTU = 23

/** 23 − 3. What a link carries until `onMtuChanged` says otherwise. */
internal const val DEFAULT_MAX_WRITE = DEFAULT_MTU - MeshBleBridge.ATT_HEADER

// --- Link state ---------------------------------------------------------------

/**
 * One ATT operation may be outstanding per link. The stack rejects the second
 * one, so we gate ourselves and report the rejection up as a retryable failure
 * rather than letting the packet vanish.
 */
internal class OpGate {
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
internal class CentralLink(val id: String, val device: BluetoothDevice) {
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

/**
 * A GATT status as something readable, keeping the number.
 *
 * These constants are not public API — `BluetoothGatt` exposes almost none of
 * them — so they are spelled out here from the stack's own `gatt_api.h`. A
 * bare `status=8` is not actionable, and working out which of these it was
 * has cost hours more than once.
 */
internal fun gattStatusName(status: Int): String {
    val name = when (status) {
        0 -> "success"
        8 -> "GATT_CONN_TIMEOUT (link supervision timed out — peer out of range or asleep)"
        19 -> "GATT_CONN_TERMINATE_PEER_USER (the peer hung up)"
        22 -> "GATT_CONN_TERMINATE_LOCAL_HOST (we hung up)"
        34 -> "GATT_CONN_LMP_TIMEOUT"
        62 -> "GATT_CONN_FAIL_ESTABLISH (never finished connecting)"
        133 -> "GATT_ERROR (the catch-all; usually a stack-level failure)"
        256 -> "GATT_CONN_CANCEL"
        else -> "unmapped"
    }
    return "status=$status $name"
}

internal class PeripheralLink(val id: String, val device: BluetoothDevice) {
    @Volatile var maxWrite = DEFAULT_MAX_WRITE
    @Volatile var announced = false
    val connectedAt = SystemClock.elapsedRealtime()

    /**
     * Largest single write this central has made to us. A write of N bytes
     * proves the connection's ATT MTU is at least N + 3, which is the only MTU
     * evidence we get on a connection whose `onMtuChanged` fired for a
     * previous incarnation of this process.
     */
    @Volatile var largestInbound = 0

    /** A delayed [adoptStaleSubscription] check is already queued. */
    val adoptionScheduled = AtomicBoolean(false)
    @Volatile var cccd: ByteArray = BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE
    val gate = OpGate()

    /**
     * Frames written to us before Rust had been told this link exists.
     *
     * A remote central connects, writes its announce, and only then enables
     * notifications — and it is that last step which makes the link writable
     * and so the moment we announce it upward. Dropping the writes in between
     * discards exactly the packet that would have introduced the peer, which
     * on a three-device mesh with rotating addresses is most of them.
     *
     * bitchat's own Android server delivers an inbound write unconditionally
     * (`BluetoothGattServerManager.onCharacteristicWriteRequest`). We cannot
     * quite do that — `MeshLink` promises that a Connected link is writable,
     * and it is not writable until the subscription lands — so the frames are
     * held and flushed the instant it becomes announced. Ordering is preserved.
     *
     * Bounded, because an unsubscribed peer that only writes is otherwise a
     * memory leak with a radio attached. Over the cap the oldest frame is
     * dropped: a mesh recovers from a lost announce on the next cadence, and
     * a stalled buffer would be a worse failure than a late one.
     */
    val pendingInbound = ArrayDeque<ByteArray>()
}
