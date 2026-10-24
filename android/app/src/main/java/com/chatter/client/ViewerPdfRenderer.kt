package com.chatter.client

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.pdf.PdfRenderer
import android.os.ParcelFileDescriptor
import android.system.Os
import java.io.File
import java.util.concurrent.atomic.AtomicLong

/** In-app PDF rendering. The private staging file is unlinked once PdfRenderer owns its fd. */
internal object ViewerPdfRenderer {
    private data class Document(val descriptor: ParcelFileDescriptor, val renderer: PdfRenderer)
    private val documents = mutableMapOf<Long, Document>()
    private val next = AtomicLong(1)

    fun startup(context: Context) {
        val folder = File(context.cacheDir, "viewer-pdf-preview")
        folder.listFiles()?.filter { it.isFile && it.name.startsWith("preview-") }?.forEach { it.delete() }
    }

    @Synchronized fun load(context: Context, bytes: ByteArray): LongArray? {
        var file: File? = null
        var descriptor: ParcelFileDescriptor? = null
        try {
            val folder = File(context.cacheDir, "viewer-pdf-preview").apply { check(mkdirs() || isDirectory) }
            Os.chmod(folder.absolutePath, 0x1c0) // 0700
            val staging = File.createTempFile("preview-", ".pdf", folder)
            file = staging
            Os.chmod(staging.absolutePath, 0x180) // 0600
            staging.outputStream().use { it.write(bytes) }
            descriptor = ParcelFileDescriptor.open(staging, ParcelFileDescriptor.MODE_READ_ONLY)
            check(staging.delete())
            file = null
            val owned = descriptor ?: return null
            val renderer = PdfRenderer(owned)
            descriptor = null
            if (renderer.pageCount == 0) {
                try { renderer.close() } finally { owned.close() }
                return null
            }
            val token = next.getAndIncrement()
            documents[token] = Document(owned, renderer)
            return longArrayOf(token, renderer.pageCount.toLong())
        } catch (_: Exception) {
            descriptor?.close()
            return null
        } finally {
            bytes.fill(0)
            file?.delete()
        }
    }

    @Synchronized fun render(context: Context, token: Long, index: Int): ByteArray? {
        val document = documents[token] ?: return null
        if (index !in 0 until document.renderer.pageCount) return null
        return try {
            val page = document.renderer.openPage(index)
            try {
                if (page.width <= 0 || page.height <= 0) return null
                val targetWidth = (context.resources.displayMetrics.widthPixels * 1.5f).toInt().coerceIn(480, 1200)
                val scale = minOf(targetWidth.toFloat() / page.width.toFloat(),
                    (targetWidth * 2).toFloat() / page.height.toFloat())
                val width = (page.width * scale).toInt().coerceAtLeast(1)
                val height = (page.height * scale).toInt().coerceAtLeast(1)
                val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
                try {
                    bitmap.eraseColor(Color.WHITE)
                    page.render(bitmap, null, null, PdfRenderer.Page.RENDER_MODE_FOR_DISPLAY)
                    val pixels = IntArray(width * height)
                    try {
                        bitmap.getPixels(pixels, 0, width, 0, 0, width, height)
                        ByteArray(8 + pixels.size * 4).also { result ->
                            for (i in 0 until 4) {
                                result[i] = (width ushr (24 - i * 8)).toByte()
                                result[4 + i] = (height ushr (24 - i * 8)).toByte()
                            }
                            for (i in pixels.indices) {
                                val argb = pixels[i]
                                val at = 8 + i * 4
                                result[at] = (argb ushr 16).toByte()
                                result[at + 1] = (argb ushr 8).toByte()
                                result[at + 2] = argb.toByte()
                                result[at + 3] = (argb ushr 24).toByte()
                            }
                        }
                    } finally {
                        pixels.fill(0)
                    }
                } finally {
                    bitmap.eraseColor(Color.TRANSPARENT)
                    bitmap.recycle()
                }
            } finally { page.close() }
        } catch (_: Exception) { null }
    }

    @Synchronized fun close(token: Long) {
        documents.remove(token)?.let {
            try { it.renderer.close() } finally { it.descriptor.close() }
        }
    }
}
