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
                beginPeripheral(device)
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
            if (value == null || value.isEmpty() || link == null) {
                BluetoothGatt.GATT_FAILURE
            } else synchronized(link) {
                val firstWrite = !link.admitted
                if (!admitFirstWrite(link, value)) {
                    BluetoothGatt.GATT_INSUFFICIENT_AUTHORIZATION
                } else {
                    val preface = firstWrite && link.admissionBeacon != null
                    if (!preface) {
                        if (link.announced) {
                            emitData(id, value.copyOf())
                        } else {
                            synchronized(link.pendingInbound) {
                                while (link.pendingInbound.size >= MAX_PENDING_INBOUND) {
                                    link.pendingInbound.removeFirst()
                                }
                                link.pendingInbound.addLast(value.copyOf())
                            }
                            if (value.size > link.largestInbound) link.largestInbound = value.size
                            val waited = SystemClock.elapsedRealtime() - link.connectedAt
                            if (!link.subscribed && link.adoptionScheduled.compareAndSet(false, true)) {
                                handler.postDelayed(
                                    { adoptStaleSubscription(link) },
                                    (SUBSCRIBE_GRACE_MS - waited).coerceAtLeast(0L),
                                )
                            }
                        }
                    }
                    if (link.subscribed) announcePeripheral(link, "subscribed and admitted")
                    BluetoothGatt.GATT_SUCCESS
                }
            }
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
        if (descriptor.uuid == CCCD_UUID && value != null && !preparedWrite && offset == 0) {
            val link = beginPeripheral(device)
            synchronized(link) {
                if (link.rejected) {
                    status = BluetoothGatt.GATT_INSUFFICIENT_AUTHORIZATION
                } else {
                    link.cccd = value.copyOf()
                    link.subscribed =
                        value.contentEquals(BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE) ||
                            value.contentEquals(BluetoothGattDescriptor.ENABLE_INDICATION_VALUE)
                    if (link.subscribed) {
                        // The ATT subscription can be prepared before admission.
                        // No app notifications or data are released until the first write is checked.
                        announcePeripheral(link, "subscribed and admitted")
                    } else if (link.announced) {
                        link.announced = false
                        emitDisconnected(link.id)
                    }
                }
            }
        } else {
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
