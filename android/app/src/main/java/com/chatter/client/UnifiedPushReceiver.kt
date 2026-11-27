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
        private const val CONSENTED = "consented_distributor"
        private const val RESET_PENDING = "reset_pending"

        private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

        /**
         * The saved distributor if the user picked it in [MainActivity]'s
         * prompt, else null. A distributor saved without that consent (builds
         * before the prompt enrolled the first one installed) is unregistered
         * here, and the user is asked like on a fresh install; the server
         * endpoint and the core's flag are cleared by [flushReset].
         */
        private fun chosenDistributor(context: Context): String? {
            val saved = UnifiedPush.getAckDistributor(context)
            val chosen = consentedDistributor(saved, prefs(context).getString(CONSENTED, null))
            if (saved != null && chosen == null) {
                Log.i(TAG, "UnifiedPush distributor saved without consent; unregistering")
                UnifiedPush.unregisterApp(context)
                prefs(context).edit().putBoolean(RESET_PENDING, true).commit()
            }
            return chosen
        }

        /**
         * Re-register with the distributor the user chose, if any. Never picks
         * one. Safe to call repeatedly (register is idempotent per distributor).
         */
        fun registerIfChosen(context: Context) {
            try {
                if (chosenDistributor(context) != null) UnifiedPush.registerApp(context)
            } catch (e: Throwable) {
                Log.w(TAG, "UnifiedPush register failed", e)
            }
            flushReset(context)
        }

        /**
         * After a consent reset, have the core clear the server endpoint and go
         * back to its own socket. Kept pending until the core is up to take it.
         */
        fun flushReset(context: Context) {
            val prefs = prefs(context)
            if (!prefs.getBoolean(RESET_PENDING, false)) return
            if (nativeResetEndpoint()) prefs.edit().remove(RESET_PENDING).apply()
        }

        /** Installed distributors the user has not been asked about yet. */
        fun distributorsToOffer(context: Context): List<String> = try {
            pickableDistributors(
                chosenDistributor(context),
                UnifiedPush.getDistributors(context),
                prefs(context).getStringSet(DECLINED, emptySet()).orEmpty(),
            )
        } catch (e: Throwable) {
            Log.w(TAG, "UnifiedPush distributor lookup failed", e)
            emptyList()
        }

        /** The user picked [distributor]: record the consent, save it and register. */
        fun choose(context: Context, distributor: String) {
            // The new endpoint replaces any one a pending reset would clear.
            prefs(context).edit()
                .putString(CONSENTED, distributor)
                .remove(RESET_PENDING)
                .commit()
            try {
                UnifiedPush.saveDistributor(context, distributor)
                UnifiedPush.registerApp(context)
            } catch (e: Throwable) {
                Log.w(TAG, "UnifiedPush enable failed", e)
            }
        }

        /** The user kept Chatter's own socket: do not ask about these again. */
        fun decline(context: Context, distributors: List<String>) {
            val prefs = prefs(context)
            val declined = prefs.getStringSet(DECLINED, emptySet()).orEmpty() + distributors
            prefs.edit().putStringSet(DECLINED, declined).apply()
        }

        /** Implemented in Rust: true once the core took the reset. */
        @JvmStatic
        private external fun nativeResetEndpoint(): Boolean

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

/** [saved] counts as chosen only if it is the distributor the user consented to. */
internal fun consentedDistributor(saved: String?, consented: String?): String? =
    saved?.takeIf { it == consented }
