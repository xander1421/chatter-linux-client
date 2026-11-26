package com.chatter.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test

class RuntimePermissionQueueTest {
    private val notifications = "android.permission.POST_NOTIFICATIONS"
    private val microphone = "android.permission.RECORD_AUDIO"
    private val camera = "android.permission.CAMERA"

    @Test
    fun notificationResultLeavesQueuedCallRequestsWaiting() {
        val queue = RuntimePermissionQueue()
        queue.enqueue(0, arrayOf(notifications))
        assertEquals(notifications, queue.startNext()?.permissions?.single())
        queue.enqueue(41, arrayOf(microphone))
        queue.enqueue(42, arrayOf(camera))
        assertNull(queue.startNext())

        assertEquals(0L, queue.completeActive()?.id)
        assertNull(queue.completeActive())
        val call = checkNotNull(queue.startNext())
        assertEquals(41L, call.id)
        assertEquals(microphone, call.permissions.single())
        assertEquals(call.id, queue.completeActive()?.id)

        val video = checkNotNull(queue.startNext())
        assertEquals(42L, video.id)
        assertEquals(camera, video.permissions.single())
        assertEquals(video.id, queue.completeActive()?.id)
        assertFalse(queue.hasRequests())
    }

    @Test
    fun cancellationReturnsActiveAndQueuedRequestIds() {
        val queue = RuntimePermissionQueue()
        queue.enqueue(43, arrayOf(microphone))
        assertEquals(43L, queue.startNext()?.id)
        queue.enqueue(44, arrayOf(camera))

        assertEquals(listOf(43L, 44L), queue.cancelAll().map { it.id })
        assertNull(queue.completeActive())
        assertFalse(queue.hasRequests())
    }
}
