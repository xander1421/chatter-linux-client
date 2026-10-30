package com.chatter.client

import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import android.util.Log
import java.security.KeyStore
import java.security.UnrecoverableKeyException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Wraps the app's at-rest secrets (SQLCipher passphrase, mesh seal key) under a
 * non-exportable AES-256-GCM key held by the Android Keystore — the approach
 * of Signal-Android's `KeyStoreHelper` / `DatabaseSecretProvider`.
 *
 * Called from Rust over JNI (`chatter-ui/src/platform/android/keystore.rs`),
 * which resolves this class through the app classloader, so the calls work
 * from the native thread before `MainActivity` has registered itself.
 *
 * One key per purpose (`alias`), so a blob sealed for one purpose cannot be
 * opened as the other.
 *
 * The key carries NO user-authentication requirement, on purpose: the app
 * opens its database at launch, and in the background for notifications and
 * the mesh, with nobody present to authenticate. What this buys is that a copy
 * of the app's files (root, forensic extraction, a backup that escaped
 * `allowBackup=false`) no longer contains the key — opening the database needs
 * this device's TEE / StrongBox. It does not protect against code running as
 * the app on an unlocked device; nothing that opens the DB unattended can.
 *
 * Blob layout: `iv (12) || ciphertext+tag`. Framing/versioning is Rust's.
 */
object KeyStoreHelper {
    private const val TAG = "ChatterKeyStore"
    private const val ANDROID_KEY_STORE = "AndroidKeyStore"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val IV_LEN = 12
    private const val TAG_BITS = 128

    @JvmStatic
    @Synchronized
    fun seal(alias: String, plaintext: ByteArray): ByteArray {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        // The Keystore picks the IV (setRandomizedEncryptionRequired is the
        // default and refuses a caller-chosen one).
        cipher.init(Cipher.ENCRYPT_MODE, getOrCreateKey(alias))
        val iv = cipher.iv
        check(iv.size == IV_LEN) { "unexpected GCM IV length ${iv.size}" }
        return iv + cipher.doFinal(plaintext)
    }

    /** Throws if the key is missing or the blob does not authenticate. Never creates a key. */
    @JvmStatic
    @Synchronized
    fun unseal(alias: String, blob: ByteArray): ByteArray {
        require(blob.size > IV_LEN + TAG_BITS / 8) { "sealed blob too short (${blob.size} bytes)" }
        val key = existingKey(alias) ?: throw IllegalStateException("no Keystore key '$alias'")
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(TAG_BITS, blob, 0, IV_LEN))
        return cipher.doFinal(blob, IV_LEN, blob.size - IV_LEN)
    }

    private fun keyStore(): KeyStore = KeyStore.getInstance(ANDROID_KEY_STORE).apply { load(null) }

    private fun existingKey(alias: String): SecretKey? {
        val ks = keyStore()
        if (!ks.containsAlias(alias)) return null
        // Signal retries once: some Keystore implementations transiently throw here.
        return try {
            ks.getKey(alias, null) as SecretKey
        } catch (e: UnrecoverableKeyException) {
            ks.getKey(alias, null) as SecretKey
        }
    }

    private fun getOrCreateKey(alias: String): SecretKey = existingKey(alias) ?: createKey(alias)

    private fun createKey(alias: String): SecretKey {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            try {
                return generate(alias, strongBox = true).also { Log.i(TAG, "key '$alias' created in StrongBox") }
            } catch (e: StrongBoxUnavailableException) {
                Log.i(TAG, "no StrongBox; key '$alias' falls back to the TEE")
            }
        }
        return generate(alias, strongBox = false).also { Log.i(TAG, "key '$alias' created in the TEE") }
    }

    private fun generate(alias: String, strongBox: Boolean): SecretKey {
        val spec = KeyGenParameterSpec.Builder(
            alias,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setKeySize(256)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setRandomizedEncryptionRequired(true)
            .setUserAuthenticationRequired(false)
            .apply { if (strongBox && Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) setIsStrongBoxBacked(true) }
            .build()
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEY_STORE)
        generator.init(spec)
        return generator.generateKey()
    }
}
