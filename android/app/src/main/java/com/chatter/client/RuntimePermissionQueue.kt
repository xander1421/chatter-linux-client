package com.chatter.client

/** Associates the single Android permission launcher with the request it is serving. */
internal class RuntimePermissionQueue {
    class Request(val id: Long, val permissions: Array<String>)

    private val pending = ArrayDeque<Request>()
    private var active: Request? = null

    fun hasRequests(): Boolean = active != null || pending.isNotEmpty()

    fun enqueue(id: Long, permissions: Array<String>) {
        pending.addLast(Request(id, permissions))
    }

    fun startNext(): Request? {
        if (active != null) return null
        return pending.removeFirstOrNull().also { active = it }
    }

    fun completeActive(): Request? {
        val completed = active
        active = null
        return completed
    }

    fun cancelAll(): List<Request> {
        val cancelled = listOfNotNull(completeActive()) + pending
        pending.clear()
        return cancelled
    }
}
