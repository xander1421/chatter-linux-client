package com.chatter.client

import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.core.content.pm.ShortcutInfoCompat
import androidx.core.content.pm.ShortcutManagerCompat

/**
 * Direct Share: publishes the chats core selected as dynamic sharing shortcuts,
 * so they show in other apps' share sheets next to apps like Telegram's.
 *
 * What is published, and when, is decided in chatter-core (`share_targets`):
 * names only, a handful of identified chats, nothing at the private default
 * notification level, nothing while signed out or locked. This object only
 * hands that list to Android and removes it again. The shortcut id is the
 * opaque per-account key, never a user or channel id.
 */
object ShareTargets {
    private const val TAG = "ChatterShareTargets"
    // Matches res/xml/shortcuts.xml.
    private const val CATEGORY = "com.chatter.client.category.SHARE_TARGET"

    /** Replace every published target with [keys]/[labels] (parallel arrays); empty removes them all. */
    fun publish(context: Context, keys: Array<String>, labels: Array<String>) {
        try {
            val cap = ShortcutManagerCompat.getMaxShortcutCountPerActivity(context)
            val shortcuts = keys.indices.take(cap).map { i ->
                ShortcutInfoCompat.Builder(context, keys[i])
                    .setShortLabel(labels[i])
                    .setLongLived(true)
                    .setCategories(setOf(CATEGORY))
                    // Required by the API; a tap on a share target delivers the
                    // share intent to MainActivity, not this one.
                    .setIntent(Intent(context, MainActivity::class.java).setAction(Intent.ACTION_MAIN))
                    .build()
            }
            if (shortcuts.isEmpty()) ShortcutManagerCompat.removeAllDynamicShortcuts(context)
            else ShortcutManagerCompat.setDynamicShortcuts(context, shortcuts)
        } catch (e: RuntimeException) {
            // Rate limits and OEM quirks must never take the app down; the
            // share sheet then simply lists Chatter without chats.
            Log.w(TAG, "publishing share targets failed: ${e.javaClass.simpleName}")
        }
    }
}
