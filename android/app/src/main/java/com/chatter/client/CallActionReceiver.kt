package com.chatter.client

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

/**
 * Receives the Accept / Decline button taps from the incoming-call
 * notification and forwards them into Rust via [nativeOnCallAction].
 *
 * Decline is fully headless. Accept additionally brings [MainActivity] to the
 * foreground so the in-app active-call screen can render (audio already
 * connects in Rust).
 */
class CallActionReceiver : BroadcastReceiver() {
    companion object {
        init { System.loadLibrary("chatter_ui") }
        const val ACTION_ACCEPT = "com.chatter.client.CALL_ACCEPT"
        const val ACTION_DECLINE = "com.chatter.client.CALL_DECLINE"
        private const val TAG = "CallActionReceiver"
    }

    private external fun nativeOnCallAction(callId: String, accept: Boolean)

    override fun onReceive(context: Context, intent: Intent) {
        val callId = intent.getStringExtra("call_id")
        if (callId.isNullOrEmpty()) {
            Log.w(TAG, "call action with no call_id")
            return
        }
        val accept = intent.action == ACTION_ACCEPT
        Log.i(TAG, "call action: accept=$accept call_id=$callId")
        nativeOnCallAction(callId, accept)

        if (accept) {
            // Bring the app forward so the active-call screen renders.
            val launch = Intent(context, MainActivity::class.java).apply {
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
                putExtra("incoming_call_id", callId)
            }
            context.startActivity(launch)
        }
    }
}
