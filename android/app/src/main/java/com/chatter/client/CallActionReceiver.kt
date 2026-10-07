package com.chatter.client

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** Headless Decline only; Accept uses an immutable activity PendingIntent. */
class CallActionReceiver : BroadcastReceiver() {
    companion object {
        init { System.loadLibrary("chatter_ui") }
        const val ACTION_DECLINE = "com.chatter.client.CALL_DECLINE"
    }

    private external fun nativeOnCallAction(callId: String, capability: String)

    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_DECLINE) return
        try {
            val callId = intent.getStringExtra("call_id") ?: return
            val capability = intent.getStringExtra("call_notification_capability") ?: return
            if (callId.isEmpty() || capability.isEmpty()) return
            nativeOnCallAction(callId, capability)
        } catch (_: RuntimeException) {
            // Malformed extras fail closed before any call action.
        }
    }
}
