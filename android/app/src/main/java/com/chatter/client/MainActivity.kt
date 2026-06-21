package com.chatter.client

import android.content.pm.PackageManager
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.util.Log
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import com.google.androidgamesdk.GameActivity
import java.io.File

/**
 * GameActivity host for the Slint/Rust client.
 *
 * The native side (`android_main`, GameActivity glue) lives in
 * `libchatter_ui.so`, loaded via the `android.app.lib_name` manifest meta-data.
 * This subclass hosts the OS-facing bridges that Slint/Rust can't do directly,
 * surfacing results back to Rust over JNI. The Rust side reaches *into* Android
 * (e.g. notifications) via `ndk-context`; Android reaches *back* via the
 * `native…` methods declared here.
 */
class MainActivity : GameActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Hand the Activity to Rust. ndk-context only exposes the Application,
        // which lacks our Activity methods (launchFilePicker, requestPermissions),
        // so Rust holds a GlobalRef to `this` for JNI callbacks. super.onCreate
        // has loaded libchatter_ui by now (android.app.lib_name), so the native
        // symbol resolves.
        nativeRegisterActivity()
    }

    /** Implemented in Rust. Stores a GlobalRef to this Activity. */
    private external fun nativeRegisterActivity()

    // --- File picker (Storage Access Framework) --------------------------------
    // Registered at construction (before onStart), per the ComponentActivity
    // contract that GameActivity inherits through AppCompatActivity.
    private val openDocuments =
        registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
            // SAF returns content:// URIs with transient read grants. Copy each
            // into our cache dir so Rust gets plain filesystem paths it can read.
            val paths = uris.mapNotNull { copyToCache(it) }
            nativeOnFilesPicked(paths.toTypedArray())
        }

    /** Called from Rust (JNI) to open the document chooser. */
    @Suppress("unused")
    fun launchFilePicker(mimeTypes: Array<String>) {
        val types = if (mimeTypes.isEmpty()) arrayOf("*/*") else mimeTypes
        runOnUiThread { openDocuments.launch(types) }
    }

    private fun copyToCache(uri: Uri): String? {
        return try {
            val name = queryDisplayName(uri) ?: "attachment_${System.nanoTime()}"
            val dir = File(cacheDir, "picked").apply { mkdirs() }
            val out = File(dir, name)
            val input = contentResolver.openInputStream(uri) ?: return null
            input.use { stream -> out.outputStream().use { stream.copyTo(it) } }
            out.absolutePath
        } catch (e: Exception) {
            Log.e(TAG, "copyToCache failed for $uri", e)
            null
        }
    }

    private fun queryDisplayName(uri: Uri): String? =
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
            ?.use { c -> if (c.moveToFirst()) c.getString(0) else null }

    /** Implemented in Rust (libchatter_ui.so). Delivers picked file paths. */
    private external fun nativeOnFilesPicked(paths: Array<String>)

    // --- Runtime permissions ---------------------------------------------------
    private val permissionRequest =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { result ->
            val granted = result.filterValues { it }.keys.toTypedArray()
            val denied = result.filterValues { !it }.keys.toTypedArray()
            nativeOnPermissionsResult(granted, denied)
        }

    /** Called from Rust (JNI). True if [permission] is already granted. */
    @Suppress("unused")
    fun hasPermission(permission: String): Boolean =
        ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED

    /** Called from Rust (JNI). Shows the system grant dialog for [permissions]. */
    @Suppress("unused")
    fun requestPermissions(permissions: Array<String>) {
        runOnUiThread { permissionRequest.launch(permissions) }
    }

    /** Implemented in Rust (libchatter_ui.so). Delivers grant results. */
    private external fun nativeOnPermissionsResult(granted: Array<String>, denied: Array<String>)

    private companion object {
        const val TAG = "ChatterFilePicker"
    }
}
