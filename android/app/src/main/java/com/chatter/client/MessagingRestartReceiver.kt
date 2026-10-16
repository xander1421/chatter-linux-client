package com.chatter.client

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.UserManager

/** Restores the messaging service after reboot or an in-place app update. */
class MessagingRestartReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED &&
            intent.action != Intent.ACTION_MY_PACKAGE_REPLACED) return
        // The database and the signed-in marker live in credential-encrypted
        // storage. Never try to open either before the first unlock.
        if (context.getSystemService(UserManager::class.java)?.isUserUnlocked != true) return
        if (MessagingService.wasSignedIn(context)) MessagingService.start(context)
    }
}
