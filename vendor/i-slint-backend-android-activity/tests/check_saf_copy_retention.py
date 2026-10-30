#!/usr/bin/env python3
"""Run MainActivity's actual copyToCache body against synthetic providers.

Coordinator-owned execution; no Android/Gradle/device/application state needed.
Set KOTLINC if the Kotlin compiler is not on PATH.
"""
import os
from pathlib import Path
import subprocess
import tempfile

client = Path(__file__).resolve().parents[3]
source = (client / "android/app/src/main/java/com/chatter/client/MainActivity.kt").read_text()
start = source.index("    private fun copyToCache(uri: Uri): String? {")
end = source.index("    private fun queryDisplayName", start)
copy_body = source[start:end]

harness = r'''
import java.io.File
import java.io.InputStream
import java.io.ByteArrayInputStream
import java.nio.file.Files

data class Uri(val name: String?, val bytes: ByteArray, val fail: Boolean = false, val missing: Boolean = false)
object Log {
    var failures = 0
    fun e(tag: String, message: String) {
        check(message == "Attachment copy failed")
        failures++
    }
}
class Resolver {
    var closed = 0
    fun openInputStream(uri: Uri): InputStream? {
        if (uri.missing) return null
        return object : ByteArrayInputStream(uri.bytes) {
            override fun read(bytes: ByteArray, offset: Int, length: Int): Int {
                if (uri.fail && pos > 0) throw java.io.IOException("private provider detail")
                return super.read(bytes, offset, if (uri.fail) 1 else length)
            }
            override fun close() { closed++; super.close() }
        }
    }
}
class Host(val cacheDir: File) {
    val contentResolver = Resolver()
    val TAG = "SyntheticProvider"
    private fun queryDisplayName(uri: Uri): String? = uri.name
    fun pick(uri: Uri): String? = copyToCache(uri)
COPY_BODY
}
fun main() {
    val cache = Files.createTempDirectory("chatter-saf-synthetic-").toFile()
    try {
        val host = Host(cache)
        val a = host.pick(Uri("image.jpg", "first".toByteArray()))!!
        val b = host.pick(Uri("image.jpg", "second".toByteArray()))!!
        val c = host.pick(Uri("image.jpg", "third".toByteArray()))!!
        check(setOf(a, b, c).size == 3) { "same-name picks aliased pending attachment paths" }
        check(File(a).readText() == "first") { "later picker changed an earlier draft's bytes" }
        check(File(b).readText() == "second")
        check(File(c).readText() == "third")
        check(listOf(a, b, c).all { File(it).name == "image.jpg" })
        val beforeFailure = File(cache, "picked").listFiles()!!.map { it.name }.toSet()
        check(host.pick(Uri("image.jpg", "partial".toByteArray(), fail = true)) == null)
        check(File(a).readText() == "first")
        check(File(cache, "picked").listFiles()!!.map { it.name }.toSet() == beforeFailure) {
            "failed provider retained a partial file/directory"
        }
        check(Log.failures == 1)
        check(host.contentResolver.closed == 4) { "provider streams were not closed" }
        check(host.pick(Uri("missing", byteArrayOf(), missing = true)) == null)
        val traversal = host.pick(Uri("../../outside.txt", "safe".toByteArray()))!!
        check(File(traversal).name == "outside.txt")
        check(File(traversal).canonicalPath.startsWith(File(cache, "picked").canonicalPath + File.separator))
        val fallback = host.pick(Uri("..", "fallback".toByteArray()))!!
        check(File(fallback).name.startsWith("attachment_"))
        println("SAF synthetic retention: same-name picks, failure cleanup, stream closure and path boundaries passed")
    } finally {
        cache.deleteRecursively()
    }
}
'''.replace("COPY_BODY", copy_body)

with tempfile.TemporaryDirectory(prefix="chatter-saf-compile-") as temporary:
    temporary = Path(temporary)
    source_file = temporary / "SafCopy.kt"
    source_file.write_text(harness)
    artifact = temporary / "saf-copy.jar"
    subprocess.run([os.environ.get("KOTLINC", "kotlinc"), str(source_file), "-include-runtime", "-d", str(artifact)], check=True)
    subprocess.run(["java", "-jar", str(artifact)], check=True)
