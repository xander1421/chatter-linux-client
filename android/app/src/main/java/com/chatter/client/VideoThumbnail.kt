package com.chatter.client

import android.graphics.Bitmap
import android.media.MediaDataSource
import android.media.MediaMetadataRetriever
import android.os.Build
import java.io.ByteArrayOutputStream
import java.io.DataOutputStream
import java.io.IOException
import java.util.concurrent.Executors
import java.util.concurrent.Semaphore
import java.util.concurrent.TimeUnit

/** Private authenticated native ranges, never a path, Uri or exported player. */
class VideoThumbnail {
    private external fun nativeSize(token: Long): Long
    private external fun nativeRead(token: Long, position: Long, buffer: ByteArray, offset: Int, count: Int): Int

    fun extract(token: Long): ByteArray? {
        // No unbounded queue; a timed-out platform decoder continues occupying this
        // sole permit until it really exits. Its native source is revoked on timeout.
        if (Build.VERSION.SDK_INT < 27 || !permit.tryAcquire()) return null
        val future = executor.submit<ByteArray?> {
            try { frame(token) } finally { permit.release() }
        }
        return try { future.get(5, TimeUnit.SECONDS) }
        catch (_: Exception) { null }
    }

    /** Local user-selected file on send. Header: five big-endian numbers,
     * followed by the bounded JPEG thumbnail. Never accepts a network URL. */
    fun extractFile(path: String): ByteArray? {
        if (Build.VERSION.SDK_INT < 27 || !permit.tryAcquire()) return null
        val future = executor.submit<ByteArray?> {
            try { fileFrame(path) } finally { permit.release() }
        }
        return try { future.get(5, TimeUnit.SECONDS) }
        catch (_: Exception) { null }
    }

    private fun fileFrame(path: String): ByteArray? {
        val retriever = MediaMetadataRetriever()
        var bitmap: Bitmap? = null
        try {
            retriever.setDataSource(path)
            val width = retriever.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_WIDTH)?.toIntOrNull() ?: return null
            val height = retriever.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_HEIGHT)?.toIntOrNull() ?: return null
            val duration = retriever.extractMetadata(MediaMetadataRetriever.METADATA_KEY_DURATION)?.toLongOrNull() ?: 0L
            if (width !in 1..8192 || height !in 1..8192 || width.toLong() * height > 16_777_216 || duration < 0) return null
            val scale = 384.0 / maxOf(width, height)
            val w = maxOf(1, (width * minOf(1.0, scale)).toInt())
            val h = maxOf(1, (height * minOf(1.0, scale)).toInt())
            // Some camera MP4s have no key frame at timestamp zero. Try the
            // first decodable frame and then the midpoint before giving up.
            bitmap = try {
                retriever.getScaledFrameAtTime(0, MediaMetadataRetriever.OPTION_CLOSEST_SYNC, w, h)
            } catch (_: Exception) { null }
            if (bitmap == null) bitmap = try {
                retriever.getScaledFrameAtTime(0, MediaMetadataRetriever.OPTION_CLOSEST, w, h)
            } catch (_: Exception) { null }
            if (bitmap == null && duration > 0) bitmap = try {
                retriever.getScaledFrameAtTime(minOf(duration, 86_400_000L) * 500, MediaMetadataRetriever.OPTION_CLOSEST_SYNC, w, h)
            } catch (_: Exception) { null }
            val bytes = ByteArrayOutputStream()
            if (bitmap != null) {
                if (bitmap.width !in 1..384 || bitmap.height !in 1..384) return null
                if (!bitmap.compress(Bitmap.CompressFormat.JPEG, 80, bytes) || bytes.size() > 256 * 1024) return null
            }
            val packed = ByteArrayOutputStream()
            DataOutputStream(packed).use { output ->
                output.writeInt(bitmap?.width ?: 0)
                output.writeInt(bitmap?.height ?: 0)
                output.writeInt(width)
                output.writeInt(height)
                output.writeLong(duration)
                output.write(bytes.toByteArray())
            }
            return packed.toByteArray()
        } catch (_: Exception) { return null }
        finally {
            bitmap?.recycle()
            try { retriever.release() } catch (_: Exception) {}
        }
    }

    private fun frame(token: Long): ByteArray? {
        val retriever = MediaMetadataRetriever()
        var bitmap: Bitmap? = null
        val source = object : MediaDataSource() {
            override fun getSize(): Long = nativeSize(token)
            override fun readAt(position: Long, buffer: ByteArray, offset: Int, size: Int): Int =
                nativeRead(token, position, buffer, offset, size)
            override fun close() {}
        }
        try {
            retriever.setDataSource(source)
            val width = retriever.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_WIDTH)?.toIntOrNull() ?: return null
            val height = retriever.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_HEIGHT)?.toIntOrNull() ?: return null
            // Refuse malformed/unreasonably large native decoder allocations.
            if (width !in 1..8192 || height !in 1..8192 || width.toLong() * height > 16_777_216) return null
            val scale = 384.0 / maxOf(width, height)
            val w = maxOf(1, (width * minOf(1.0, scale)).toInt())
            val h = maxOf(1, (height * minOf(1.0, scale)).toInt())
            bitmap = try {
                retriever.getScaledFrameAtTime(0, MediaMetadataRetriever.OPTION_CLOSEST_SYNC, w, h)
            } catch (_: Exception) { null }
            if (bitmap == null) bitmap = try {
                retriever.getScaledFrameAtTime(0, MediaMetadataRetriever.OPTION_CLOSEST, w, h)
            } catch (_: Exception) { null }
            if (bitmap == null) {
                val duration = retriever.extractMetadata(MediaMetadataRetriever.METADATA_KEY_DURATION)?.toLongOrNull() ?: 0L
                if (duration > 0) bitmap = try {
                    retriever.getScaledFrameAtTime(minOf(duration, 86_400_000L) * 500, MediaMetadataRetriever.OPTION_CLOSEST_SYNC, w, h)
                } catch (_: Exception) { null }
            }
            if (bitmap == null) return null
            if (bitmap.width !in 1..384 || bitmap.height !in 1..384) return null
            val output = object : ByteArrayOutputStream() {
                override fun write(b: ByteArray, off: Int, len: Int) {
                    if (len < 0 || count > 256 * 1024 - len) throw IOException("thumbnail budget exceeded")
                    super.write(b, off, len)
                }
                override fun write(b: Int) {
                    if (count >= 256 * 1024) throw IOException("thumbnail budget exceeded")
                    super.write(b)
                }
            }
            if (!bitmap.compress(Bitmap.CompressFormat.JPEG, 80, output)) return null
            return output.toByteArray()
        } catch (_: Exception) { return null }
        finally {
            bitmap?.recycle()
            try { retriever.release() } catch (_: Exception) {}
            source.close()
        }
    }

    companion object {
        private val permit = Semaphore(1)
        private val executor = Executors.newSingleThreadExecutor { task ->
            Thread(task, "chatter-video-preview").apply { isDaemon = true }
        }
    }
}
