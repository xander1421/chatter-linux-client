package com.chatter.client

import android.content.Context
import android.util.Log
import org.unifiedpush.android.connector.MessagingReceiver
import org.unifiedpush.android.connector.UnifiedPush

/**
 * Receives UnifiedPush events from a distributor app (e.g. ntfy) and bridges
 * them into the Rust core. Once the user has picked a distributor the core drops
 * its own ntfy SSE socket (the battery win); until then, and with none
 * installed, none of this fires and Chatter keeps its own socket (the default,
 * Signal-style path). A distributor sees when wakes arrive, so it is never
 * enrolled without the user choosing it.
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

        private const val PREFS = "unified_push"
        private const val DECLINED = "declined_distributors"

        /**
         * Re-register with the distributor the user chose, if any. Never picks
         * one: an installed distributor is only used after the user says so in
         * [MainActivity]'s prompt. Safe to call repeatedly (register is
         * idempotent per distributor).
         */
        fun registerIfChosen(context: Context) {
            try {
                if (UnifiedPush.getAckDistributor(context) != null) UnifiedPush.registerApp(context)
            } catch (e: Throwable) {
                Log.w(TAG, "UnifiedPush register failed", e)
            }
        }

        /** Installed distributors the user has not been asked about yet. */
        fun distributorsToOffer(context: Context): List<String> = try {
            pickableDistributors(
                UnifiedPush.getAckDistributor(context),
                UnifiedPush.getDistributors(context),
                context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                    .getStringSet(DECLINED, emptySet()).orEmpty(),
            )
        } catch (e: Throwable) {
            Log.w(TAG, "UnifiedPush distributor lookup failed", e)
            emptyList()
        }

        /** The user picked [distributor]: save it and register. */
        fun choose(context: Context, distributor: String) {
            try {
                UnifiedPush.saveDistributor(context, distributor)
                UnifiedPush.registerApp(context)
            } catch (e: Throwable) {
                Log.w(TAG, "UnifiedPush enable failed", e)
            }
        }

        /** The user kept Chatter's own socket: do not ask about these again. */
        fun decline(context: Context, distributors: List<String>) {
            val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            val declined = prefs.getStringSet(DECLINED, emptySet()).orEmpty() + distributors
            prefs.edit().putStringSet(DECLINED, declined).apply()
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

/**
 * Which distributors to offer: none once one is chosen, otherwise every
 * installed one the user has not already declined.
 */
internal fun pickableDistributors(
    chosen: String?,
    installed: List<String>,
    declined: Set<String>,
): List<String> = if (chosen != null) emptyList() else installed.filterNot { it in declined }
