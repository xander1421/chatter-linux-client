package com.chatter.client

import android.annotation.SuppressLint
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothProfile
import android.util.Log

/** Client GATT events; state and JNI dispatch remain owned by MeshBleBridge. */
@SuppressLint("MissingPermission") // Permission checked in start() and guarded by the bridge.
internal fun MeshBleBridge.createGattClientCallback(): BluetoothGattCallback = object : BluetoothGattCallback() {

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
        } else if (newState == BluetoothProfile.STATE_DISCONNECTED ||
            status != BluetoothGatt.GATT_SUCCESS) {
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
                // Warn, not debug. This is the line that says why a link we
                // were using went away, and it is the first thing anyone
                // looks for when the mesh goes quiet.
                Log.w(TAG, "central link $id disconnected, ${gattStatusName(status)}")
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
