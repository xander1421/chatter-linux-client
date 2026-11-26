package com.chatter.client

/** Standalone JVM regression for result routing through the Android request queue. */
object RuntimePermissionQueueTest {
    @JvmStatic
    fun main(args: Array<String>) {
        val queue = RuntimePermissionQueue()
        val notifications = "android.permission.POST_NOTIFICATIONS"
        val microphone = "android.permission.RECORD_AUDIO"
        val camera = "android.permission.CAMERA"

        queue.enqueue(0, arrayOf(notifications))
        check(queue.startNext()?.permissions?.single() == notifications)
        queue.enqueue(41, arrayOf(microphone))
        queue.enqueue(42, arrayOf(camera))
        check(queue.startNext() == null)

        // The notification result carries no waiter ID, even with the call queued.
        check(queue.completeActive()?.id == 0L)
        // A callback without an active request cannot claim the queued waiter.
        check(queue.completeActive() == null)
        val call = checkNotNull(queue.startNext())
        check(call.id == 41L && call.permissions.single() == microphone)
        check(queue.completeActive()?.id == call.id)

        val video = checkNotNull(queue.startNext())
        check(video.id == 42L && video.permissions.single() == camera)
        check(queue.completeActive()?.id == video.id)
        check(!queue.hasRequests())

        queue.enqueue(43, arrayOf(microphone))
        check(queue.startNext()?.id == 43L)
        queue.enqueue(44, arrayOf(camera))
        check(queue.cancelAll().map { it.id } == listOf(43L, 44L))
        check(queue.completeActive() == null && !queue.hasRequests())
        println("RuntimePermissionQueue regression passed")
    }
}
