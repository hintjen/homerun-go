package app.gethomerun.mobile

import android.content.SharedPreferences
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * The two bearer tokens, encrypted at rest.
 *
 * `MODE_PRIVATE` preferences are already unreadable by other apps, and
 * `android:allowBackup="false"` keeps them out of adb backups and out of
 * Google's. What was left is root and forensic: an unlocked bootloader, a
 * hostile recovery, a phone handed to someone with a cable. For an app holding
 * an account token that is worth twenty lines, which is the same argument
 * [TokenStore] on iOS already makes for the Keychain — this is the Android half
 * that was missing.
 *
 * # Why not EncryptedSharedPreferences
 *
 * It is the obvious answer and it is deprecated: Google deprecated
 * `androidx.security:security-crypto` in April 2025 at 1.1.0-alpha07. The
 * reasons it went are reasons to avoid it here too — it does its I/O on the
 * calling thread, and its keyset gets corrupted on some OEM devices in a way
 * that throws from the *constructor*, which is unrecoverable at the one moment
 * you cannot afford it. It also drags in Tink for what is four Keystore calls.
 *
 * The Keystore itself is not deprecated; only the wrapper is. `minSdk` is 26,
 * so hardware-backed AES-GCM is present on every device this app installs on
 * and there is no fallback path to maintain.
 *
 * # What is protected, and what deliberately is not
 *
 * The user's credentials blob and the device token. Not the API base, not the
 * PostHog id, not the device id or group id — those are identifiers the server
 * already knows and encrypting them would buy nothing while making them
 * unreadable to a `run-as` debugging session that has every right to see them.
 *
 * # Failure is a logout, not a crash
 *
 * A value that will not decrypt reads as absent, so the app treats it as "not
 * signed in" and the user signs in again. That is the whole recovery path, and
 * it is the deliberate answer to the corruption mode that makes
 * EncryptedSharedPreferences throw: losing a token costs one login, and a host
 * that cannot start costs the server it was hosting.
 */
object SecretStore {

    /**
     * Read and decrypt, migrating a plaintext value written by an older build.
     *
     * The migration is on read rather than at startup because it needs no
     * inventory of keys and no ordering against [init]-style setup: the first
     * read of each secret after the update rewrites it, and there are only two.
     */
    fun read(prefs: SharedPreferences, key: String): String? {
        val stored = prefs.getString(key, null) ?: return null

        if (!stored.startsWith(PREFIX)) {
            // Written before this existed. Return it — the user stays signed
            // in across the update — and take the opportunity to seal it.
            Log.i(TAG, "migrating \"$key\" to encrypted storage")
            write(prefs, key, stored)
            return stored
        }

        return try {
            val blob = Base64.decode(stored.removePrefix(PREFIX), Base64.NO_WRAP)
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(
                Cipher.DECRYPT_MODE,
                key(),
                GCMParameterSpec(TAG_BITS, blob, 0, IV_BYTES),
            )
            String(cipher.doFinal(blob, IV_BYTES, blob.size - IV_BYTES), Charsets.UTF_8)
        } catch (err: Exception) {
            // The key is gone or the bytes are not what it sealed. Say so
            // once, loudly: the symptom is an unexplained trip back to the
            // login screen, and this line is the only thing that separates
            // that from a server-side session expiry.
            Log.e(TAG, "could not decrypt \"$key\"; treating it as absent: ${err.message}")
            prefs.edit().remove(key).apply()
            null
        }
    }

    /** Encrypt and store, or remove the key when [value] is null. */
    fun write(prefs: SharedPreferences, key: String, value: String?) {
        if (value == null) {
            prefs.edit().remove(key).apply()
            return
        }

        val sealed = seal(value) ?: run {
            // Every Keystore path failed, including regenerating the key. The
            // choice is between an app that cannot log in and one that stores
            // a token the way every build before this one did. Storing it
            // keeps the app usable and keeps this device no worse off than it
            // was, and the ERROR is what makes the downgrade visible rather
            // than something to discover later.
            Log.e(TAG, "the keystore is unusable; storing \"$key\" unencrypted")
            prefs.edit().putString(key, value).apply()
            return
        }

        prefs.edit().putString(key, sealed).apply()
    }

    private fun seal(value: String): String? {
        encrypt(value)?.let { return it }
        // One retry, because the failure this recovers from is a key that
        // exists but no longer works — the same OEM corruption that takes
        // EncryptedSharedPreferences down. Dropping it and generating a new
        // one costs whatever was sealed under it, which is a login.
        Log.w(TAG, "re-creating the storage key after an encryption failure")
        synchronized(this) {
            cached = null
            runCatching { keystore().deleteEntry(ALIAS) }
        }
        return encrypt(value)
    }

    private fun encrypt(value: String): String? = try {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, key())
        // GCM must never reuse an IV under one key, so the Keystore generates
        // it and we store what it chose rather than supplying our own.
        val iv = cipher.iv
        check(iv.size == IV_BYTES) { "unexpected GCM IV length ${iv.size}" }
        val body = cipher.doFinal(value.toByteArray(Charsets.UTF_8))
        PREFIX + Base64.encodeToString(iv + body, Base64.NO_WRAP)
    } catch (err: Exception) {
        Log.w(TAG, "could not encrypt: ${err.message}")
        null
    }

    /**
     * The one AES key, created on first use and never leaving the Keystore.
     *
     * Synchronised because [DeviceRegistry] writes from an IO coroutine while
     * the bridge reads from whichever thread called it, and two threads
     * generating the key at once would leave one of them holding a handle to
     * an entry the other had already replaced.
     */
    @Synchronized
    private fun key(): SecretKey {
        cached?.let { return it }

        val store = keystore()
        val existing = (store.getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry)?.secretKey
        val resolved = existing ?: KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, PROVIDER)
            .apply {
                init(
                    KeyGenParameterSpec.Builder(
                        ALIAS,
                        KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                    )
                        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                        .setKeySize(256)
                        // Deliberately not `setUserAuthenticationRequired`. The
                        // host reads the device token from a foreground service
                        // to heartbeat while the screen is locked and a server
                        // is running; a key that needs the lock screen would
                        // fail exactly then.
                        .build(),
                )
            }
            .generateKey()

        cached = resolved
        return resolved
    }

    private fun keystore(): KeyStore = KeyStore.getInstance(PROVIDER).apply { load(null) }

    @Volatile
    private var cached: SecretKey? = null

    private const val TAG = "HomerunSecrets"
    private const val PROVIDER = "AndroidKeyStore"
    private const val ALIAS = "homerun-secrets"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"

    /**
     * Marks a value as sealed, and is how a plaintext value from an older
     * build is recognised. A stored token is a JWT or a hex id, so nothing
     * legitimate has ever started with this.
     */
    private const val PREFIX = "aesgcm:"

    private const val IV_BYTES = 12
    private const val TAG_BITS = 128
}
