package com.chatter.client

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.widget.Toast
import java.io.File
import java.security.MessageDigest
import java.util.concurrent.Executors

/** Final Android gate after Rust verified the signed manifest and APK digest. */
object UpdateInstaller {
    private val worker = Executors.newSingleThreadExecutor()

    fun start(activity: MainActivity, path: String): Boolean {
        if (!activity.packageManager.canRequestPackageInstalls()) {
            activity.runOnUiThread {
                activity.startActivity(Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                    Uri.parse("package:${activity.packageName}")))
                Toast.makeText(activity, "Allow installs from Chatter, then tap Update again", Toast.LENGTH_LONG).show()
            }
            return false
        }
        worker.execute {
            try {
                val apk = File(path).canonicalFile
                val privateDir = File(activity.filesDir, "data/updates").canonicalFile
                require(apk.parentFile == privateDir && apk.isFile) { "Update is outside private staging" }
                val manager = activity.packageManager
                val archive = if (Build.VERSION.SDK_INT >= 33) {
                    manager.getPackageArchiveInfo(apk.path,
                        PackageManager.PackageInfoFlags.of(PackageManager.GET_SIGNING_CERTIFICATES.toLong()))
                } else {
                    @Suppress("DEPRECATION")
                    manager.getPackageArchiveInfo(apk.path,
                        if (Build.VERSION.SDK_INT >= 28) PackageManager.GET_SIGNING_CERTIFICATES
                        else PackageManager.GET_SIGNATURES)
                } ?: error("Downloaded APK is invalid")
                require(archive.packageName == activity.packageName) { "Update package ID differs" }
                val installed = if (Build.VERSION.SDK_INT >= 33) {
                    manager.getPackageInfo(activity.packageName,
                        PackageManager.PackageInfoFlags.of(PackageManager.GET_SIGNING_CERTIFICATES.toLong()))
                } else {
                    @Suppress("DEPRECATION")
                    manager.getPackageInfo(activity.packageName,
                        if (Build.VERSION.SDK_INT >= 28) PackageManager.GET_SIGNING_CERTIFICATES
                        else PackageManager.GET_SIGNATURES)
                }
                fun code(info: android.content.pm.PackageInfo): Long =
                    if (Build.VERSION.SDK_INT >= 28) info.longVersionCode else info.versionCode.toLong()
                require(code(archive) > code(installed)) { "Update version is not newer" }
                fun signerDigests(info: android.content.pm.PackageInfo): List<String> {
                    val signatures = if (Build.VERSION.SDK_INT >= 28)
                        info.signingInfo?.apkContentsSigners?.toList() ?: emptyList()
                    else info.signatures?.toList() ?: emptyList()
                    return signatures.map {
                        MessageDigest.getInstance("SHA-256").digest(it.toByteArray())
                            .joinToString("") { byte -> "%02x".format(byte) }
                    }.sorted()
                }
                val installedSigners = signerDigests(installed)
                require(installedSigners.isNotEmpty()) { "Installed app signing certificate is missing" }
                val archiveSigners = signerDigests(archive)
                if (archiveSigners.isEmpty()) {
                    // Android 10 returns no signer info for a downloaded archive that
                    // carries only a v2 signature (S9, ui161 -> ui162). The package
                    // installer still refuses an update not signed by the installed
                    // app's key (INSTALL_FAILED_UPDATE_INCOMPATIBLE), so continuity is
                    // enforced there; this early check is the defence in depth.
                    android.util.Log.w("UpdateInstaller",
                        "archive signers unavailable on API ${Build.VERSION.SDK_INT}; relying on installer signer check")
                } else {
                    require(archiveSigners == installedSigners) {
                        "Update APK signing certificate differs from installed app"
                    }
                }
                val installer = manager.packageInstaller
                val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
                params.setAppPackageName(activity.packageName)
                params.setSize(apk.length())
                val sessionId = installer.createSession(params)
                try {
                    installer.openSession(sessionId).use { session ->
                        session.openWrite("base.apk", 0, apk.length()).use { output ->
                            apk.inputStream().use { it.copyTo(output) }
                            session.fsync(output)
                        }
                        val intent = Intent(activity, UpdateInstallReceiver::class.java)
                        val pending = PendingIntent.getBroadcast(activity, sessionId, intent,
                            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE)
                        session.commit(pending.intentSender)
                    }
                } catch (error: Exception) {
                    installer.abandonSession(sessionId)
                    throw error
                }
            } catch (error: Exception) {
                android.util.Log.w("UpdateInstaller", "update install refused before the system installer", error)
                activity.runOnUiThread {
                    Toast.makeText(activity, error.message ?: "Update install failed", Toast.LENGTH_LONG).show()
                }
                UpdateInstallReceiver.reportFailure(error.message ?: "")
            }
        }
        return true
    }
}

class UpdateInstallReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)
        if (status == PackageInstaller.STATUS_PENDING_USER_ACTION) {
            val confirmation = if (Build.VERSION.SDK_INT >= 33) {
                intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra<Intent>(Intent.EXTRA_INTENT)
            }
            if (confirmation != null) {
                confirmation.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                context.startActivity(confirmation)
            }
        } else if (status != PackageInstaller.STATUS_SUCCESS) {
            val reason = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)
            Toast.makeText(context, reason ?: "Update install failed", Toast.LENGTH_LONG).show()
            android.util.Log.w("UpdateInstaller", "system installer status $status: $reason")
            reportFailure(reason ?: "")
        }
    }

    companion object {
        /** Put the sidebar update pill into its error state with [reason]. */
        fun reportFailure(reason: String) {
            // The library is loaded whenever an update was started from the app.
            try { nativeOnInstallFailed(reason) } catch (_: UnsatisfiedLinkError) {}
        }

        @JvmStatic
        private external fun nativeOnInstallFailed(reason: String)
    }
}
