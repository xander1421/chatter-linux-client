package com.chatter.client

/** One balanced SCO client request. Broadcast state is separate from intent. */
internal class LegacyScoSession {
    enum class State { DISCONNECTED, CONNECTING, CONNECTED, UNKNOWN }

    var requested = false
        private set
    var connected = false
        private set
    private var lastState = State.UNKNOWN

    /** Repeated inventory callbacks must not acquire another SCO reference. */
    fun begin(): Boolean {
        if (requested) return false
        requested = true
        return true
    }

    /** Stop even if setup never reached CONNECTED or the platform flag is off. */
    fun end(): Boolean {
        val hadRequest = requested
        requested = false
        connected = false
        lastState = State.UNKNOWN
        return hadRequest
    }

    /** Sticky registration reports history; only a live loss fails a request. */
    fun update(state: State, previous: State, sticky: Boolean): Boolean {
        val wasActive = lastState == State.CONNECTING || lastState == State.CONNECTED
        lastState = state
        connected = state == State.CONNECTED
        return requested && !sticky && state == State.DISCONNECTED &&
            (wasActive || previous == State.CONNECTING || previous == State.CONNECTED)
    }
}
