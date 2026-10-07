package com.chatter.client

import android.app.DownloadManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.os.ParcelFileDescriptor
import android.os.PersistableBundle
import android.provider.MediaStore
import androidx.core.content.FileProvider
import java.io.File
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong

/** Explicit exports use a detached descriptor: Rust streams authenticated ranges. */
internal object ViewerMediaExport {
    private data class Pending(val uri: Uri?, val file: File?, val mime: String, val copy: Boolean)
    private val pending = ConcurrentHashMap<Long, Pending>()
    private val next = AtomicLong(1)
    @Volatile private var lastLegacyDownload: Pair<Uri, String>? = null

    fun begin(context: Context, name: String, mime: String, copy: Boolean): LongArray? {
        val safeName = name.substringAfterLast('/').substringAfterLast('\\')
            .filterNot { it.isISOControl() }.let { value ->
                var end = value.length
                while (end > 0 && value.substring(0, end).toByteArray(Charsets.UTF_8).size > 180) {
                    end -= if (end >= 2 && Character.isLowSurrogate(value[end - 1]) && Character.isHighSurrogate(value[end - 2])) 2 else 1
                }
                value.substring(0, end)
            }
            .takeIf { it.isNotBlank() && it != "." && it != ".." } ?: "media"
        if (copy && !mime.startsWith("image/")) return null
        val token = next.getAndIncrement()
        if (token <= 0) return null
        var uri: Uri? = null
        var file: File? = null
        return try {
            val descriptor: ParcelFileDescriptor
            if (!copy && Build.VERSION.SDK_INT >= 29) {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, safeName)
                    put(MediaStore.Downloads.MIME_TYPE, mime)
                    put(MediaStore.Downloads.RELATIVE_PATH, "${Environment.DIRECTORY_DOWNLOADS}/Chatter")
                    put(MediaStore.Downloads.IS_PENDING, 1)
                }
                uri = context.contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: return null
                descriptor = context.contentResolver.openFileDescriptor(uri, "w") ?: error("Output unavailable")
            } else {
                val base = if (copy) context.cacheDir
                    else context.getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS) ?: return null
                val folder = File(base, if (copy) "viewer-clipboard" else "Chatter").apply { check(mkdirs() || isDirectory) }
                val clipboardExtension = if (copy) when (mime.substringBefore(';').lowercase()) {
                    "image/png" -> ".png"; "image/jpeg" -> ".jpg"; "image/webp" -> ".webp"
                    "image/gif" -> ".gif"; "image/bmp" -> ".bmp"; "image/avif" -> ".avif"
                    "image/tiff" -> ".tif"; "image/heic" -> ".heic"; "image/heif" -> ".heif"
                    else -> return null
                } else ""
                file = File(folder, "${UUID.randomUUID()}-$safeName$clipboardExtension")
                descriptor = ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_WRITE_ONLY or ParcelFileDescriptor.MODE_CREATE or ParcelFileDescriptor.MODE_TRUNCATE)
            }
            pending[token] = Pending(uri, file, mime, copy)
            descriptor.use { longArrayOf(token, it.detachFd().toLong()) }
        } catch (_: Exception) {
            pending.remove(token)
            if (uri != null) try { context.contentResolver.delete(uri, null, null) } catch (_: Exception) {}
            file?.delete()
            null
        }
    }

    fun finish(context: Context, token: Long, success: Boolean): Boolean {
        val item = pending.remove(token) ?: return false
        fun discard() {
            if (item.uri != null) try { context.contentResolver.delete(item.uri, null, null) } catch (_: Exception) {}
            item.file?.delete()
        }
        if (!success) { discard(); return false }
        return try {
            if (item.copy) {
                val uri = FileProvider.getUriForFile(context, "${context.packageName}.viewer-media", item.file ?: error("File unavailable"))
                val clip = ClipData.newUri(context.contentResolver, "Chatter image", uri)
                if (Build.VERSION.SDK_INT >= 33) clip.description.extras = PersistableBundle().apply {
                    putBoolean("android.content.extra.IS_SENSITIVE", true)
                }
                context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)
            } else if (item.uri != null && Build.VERSION.SDK_INT >= 29) {
                val published = ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }
                check(context.contentResolver.update(item.uri, published, null, null) == 1)
            } else {
                val uri = FileProvider.getUriForFile(context, "${context.packageName}.viewer-media", item.file ?: error("File unavailable"))
                lastLegacyDownload = uri to item.mime
            }
            true
        } catch (_: Exception) { discard(); false }
    }

    fun openDownloads(context: Context) {
        if (Build.VERSION.SDK_INT >= 29) context.startActivity(Intent(DownloadManager.ACTION_VIEW_DOWNLOADS))
        else {
            val (uri, mime) = lastLegacyDownload ?: error("Download a file first")
            context.startActivity(Intent(Intent.ACTION_VIEW).setDataAndType(uri, mime)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
        }
    }
}
