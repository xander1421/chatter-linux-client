package com.chatter.client

import android.annotation.SuppressLint
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattServerCallback
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothProfile
import android.os.SystemClock
import android.util.Log

/** Server GATT events; state and JNI dispatch remain owned by MeshBleBridge. */
@SuppressLint("MissingPermission") // Permission checked in start() and guarded by the bridge.
internal fun MeshBleBridge.createGattServerCallback(): BluetoothGattServerCallback = object : BluetoothGattServerCallback() {

    override fun onServiceAdded(status: Int, service: BluetoothGattService?) {
        if (status != BluetoothGatt.GATT_SUCCESS) {
            Log.e(TAG, "onServiceAdded failed: status=$status")
            return
        }
        // Off the binder thread before taking the lifecycle lock.
        handler.post {
            synchronized(lock) {
                if (started && gattServer != null) {
                    serviceReady = true
                    beginAdvertisingLocked()
                }
            }
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
                // Said out loud, always. This branch was silent, and a link
                // dying for no stated reason is the single most expensive
                // shape of bug in this codebase: the peer's own log is the
                // only place the reason exists, and it was throwing it away.
                // Warn when the stack gives a reason, because a clean
                // teardown is routine and an aborted one is not.
                val detail = "peripheral link $id disconnected, ${gattStatusName(status)}" +
                    if (removed == null) " (no link was tracked)" else ""
                if (status == BluetoothGatt.GATT_SUCCESS) {
                    Log.d(TAG, detail)
                } else {
                    Log.w(TAG, detail)
                }
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
                } else if (link != null) {
                    // Connected but not yet announced upward: hold it rather
                    // than lose it. See `PeripheralLink.pendingInbound`.
                    synchronized(link.pendingInbound) {
                        while (link.pendingInbound.size >= MAX_PENDING_INBOUND) {
                            link.pendingInbound.removeFirst()
                            Log.d(TAG, "pending inbound full for $id, oldest frame dropped")
                        }
                        link.pendingInbound.addLast(value.copyOf())
                    }
                    if (value.size > link.largestInbound) link.largestInbound = value.size
                    Log.d(TAG, "write from $id held until the link is announced")
                    val waited = SystemClock.elapsedRealtime() - link.connectedAt
                    if (waited >= SUBSCRIBE_GRACE_MS) {
                        adoptStaleSubscription(link)
                    } else if (link.adoptionScheduled.compareAndSet(false, true)) {
                        handler.postDelayed(
                            { adoptStaleSubscription(link) },
                            SUBSCRIBE_GRACE_MS - waited,
                        )
                    }
                } else {
                    // No link object at all: already torn down, nowhere to put it.
                    Log.d(TAG, "write from $id on an unknown link, dropped")
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
                announcePeripheral(link, "subscribed")
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
