package com.chatter.client

/** Standalone JVM regression: no Android runtime, audio devices or JUnit needed. */
fun main() {
    check(System.getenv("CHATTER_NULL_AUDIO") == "1")
    val sco = LegacyScoSession()
    check(sco.begin())
    check(!sco.begin()) // Duplicate request keeps one reference and deadline.
    check(!sco.update(LegacyScoSession.State.DISCONNECTED, LegacyScoSession.State.CONNECTED, sticky = true))
    check(sco.requested && !sco.connected) // Historical disconnect is not refusal.
    check(!sco.update(LegacyScoSession.State.CONNECTING, LegacyScoSession.State.DISCONNECTED, sticky = false))
    check(!sco.connected) // Starting SCO never acknowledges applied Bluetooth.
    check(!sco.update(LegacyScoSession.State.CONNECTED, LegacyScoSession.State.CONNECTING, sticky = false))
    check(sco.connected)
    check(sco.update(LegacyScoSession.State.DISCONNECTED, LegacyScoSession.State.CONNECTED, sticky = false))
    check(!sco.connected) // Real remote loss MUST be reported as failure.
    check(sco.end())
    check(!sco.end()) // Release exactly once, even if the system flag was off.
    check(!sco.update(LegacyScoSession.State.DISCONNECTED, LegacyScoSession.State.CONNECTED, sticky = false))
    check(sco.begin()) // Explicit retry can acquire again after failure cleanup.
    check(!sco.update(LegacyScoSession.State.CONNECTING, LegacyScoSession.State.UNKNOWN, sticky = false))
    check(sco.update(LegacyScoSession.State.DISCONNECTED, LegacyScoSession.State.UNKNOWN, sticky = false))
    check(sco.end()) // Stop during setup still releases the outstanding request.
    check(!sco.update(LegacyScoSession.State.CONNECTED, LegacyScoSession.State.CONNECTING, sticky = false))
    check(!sco.requested)
    println("LegacyScoSession regression passed")
}
