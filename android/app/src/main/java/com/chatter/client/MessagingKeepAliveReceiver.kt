package com.chatter.client

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.UserManager

/** Doze-safe heartbeat: re-arms the messaging service if the OS suspended or
 * killed it while the app was backgrounded. The in-process ntfy SSE listener
 * only lives as long as this service's process, so a periodic wake-and-confirm
 * is how delivery survives Doze without an external push distributor. */
class MessagingKeepAliveReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        // Credential-encrypted storage (DB + signed-in marker) is unreadable
        // before first unlock; never touch it then.
        if (context.getSystemService(UserManager::class.java)?.isUserUnlocked != true) return
        if (MessagingService.wasSignedIn(context)) {
            // start() re-arms the next heartbeat, so the chain is self-sustaining.
            MessagingService.start(context)
        } else {
            MessagingService.cancelKeepAlive(context)
        }
    }
}
