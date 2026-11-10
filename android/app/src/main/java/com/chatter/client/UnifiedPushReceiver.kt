package com.chatter.client

import android.content.Context
import android.util.Log
import org.unifiedpush.android.connector.MessagingReceiver
import org.unifiedpush.android.connector.UnifiedPush

/**
 * Receives UnifiedPush events from a distributor app (e.g. ntfy) and bridges
 * them into the Rust core. When a distributor is present the core drops its own
 * ntfy SSE socket (the battery win); with no distributor installed none of this
 * fires and Chatter keeps its own socket (the default, Signal-style path).
 *
 * The native methods are no-ops if the core process is not yet running; a wake
 * additionally boots MessagingService so a cold process catches up via its inbox
 * poll, and the distributor re-emits the endpoint on the next registration.
 */
class UnifiedPushReceiver : MessagingReceiver() {
    override fun onNewEndpoint(context: Context, endpoint: String, instance: String) {
        Log.i(TAG, "UnifiedPush endpoint received")
        // Make sure the core is running so it can register the endpoint and
        // suppress its own socket.
        MessagingService.start(context)
        nativeOnEndpoint(endpoint)
    }

    override fun onMessage(context: Context, message: ByteArray, instance: String) {
        // Boot the core (if the process was killed) so its inbox poll runs; the
        // native nudge coalesces with it when the core is already up.
        MessagingService.start(context)
        nativeOnMessage()
    }

    override fun onRegistrationFailed(context: Context, instance: String) {
        Log.w(TAG, "UnifiedPush registration failed")
        nativeOnUnregistered()
    }

    override fun onUnregistered(context: Context, instance: String) {
        Log.i(TAG, "UnifiedPush unregistered")
        nativeOnUnregistered()
    }

    private external fun nativeOnEndpoint(endpoint: String)
    private external fun nativeOnMessage()
    private external fun nativeOnUnregistered()

    companion object {
        private const val TAG = "UnifiedPushReceiver"

        /**
         * Opt in to UnifiedPush when a distributor is installed: pick the saved
         * (or first available) distributor and register. No distributor present
         * means Chatter keeps its own socket — nothing changes. Safe to call
         * repeatedly (register is idempotent per distributor).
         */
        fun enableIfDistributorPresent(context: Context) {
            try {
                val saved = UnifiedPush.getAckDistributor(context)
                if (saved != null) {
                    UnifiedPush.registerApp(context)
                    return
                }
                val distributors = UnifiedPush.getDistributors(context)
                if (distributors.isEmpty()) return
                UnifiedPush.saveDistributor(context, distributors.first())
                UnifiedPush.registerApp(context)
            } catch (e: Throwable) {
                Log.w(TAG, "UnifiedPush enable failed", e)
            }
        }

        /** Stop using UnifiedPush and fall back to the own-SSE socket. */
        fun disable(context: Context) {
            try {
                UnifiedPush.unregisterApp(context)
            } catch (e: Throwable) {
                Log.w(TAG, "UnifiedPush disable failed", e)
            }
        }
    }
}
