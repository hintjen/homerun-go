package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import java.io.File
import java.util.zip.ZipInputStream

/**
 * Finds the bundled JVM.
 *
 * Two Android rules decide this entire design, and both are absolute:
 *
 * 1. **Since API 29 an app may not `exec()` a file from writable storage.**
 *    Anything executable must come out of the APK, in `nativeLibraryDir`. A
 *    JRE extracted to `filesDir` and run from there fails with `EACCES` no
 *    matter what permissions you set on it.
 * 2. **The packager only ships `jniLibs` entries named `lib*.so`.** A file
 *    called `java` is silently dropped from the APK — the same class of
 *    silent omission as the `_next/` asset filter.
 *
 * So the launcher binary ships as `libjavabin.so` and is executed from
 * `nativeLibraryDir`, while the rest of the runtime — `lib/modules`, the
 * class library, its own shared objects — rides along as an asset archive and
 * is unpacked once into `filesDir/jre`. That split is not a preference; it is
 * the only arrangement Android allows.
 *
 * Nothing here downloads a JRE. Which runtime to bundle is a licensing and
 * size decision, not a code one — see `docs/android-server-backend.md`.
 */
object JavaRuntime {

    /** The `java` launcher, renamed so the APK packager keeps it. */
    private const val LAUNCHER = "libjavabin.so"

    /** `assets/jre-<abi>.zip`, unpacked once into [home]. */
    private const val ASSET_PREFIX = "jre-"

    /** Written after a successful unpack so it happens once, not per launch. */
    private const val STAMP = ".unpacked"

    /** The executable, or null when no JRE is bundled for this ABI. */
    fun launcher(context: Context): File? {
        val file = File(context.applicationInfo.nativeLibraryDir, LAUNCHER)
        return file.takeIf { it.canExecute() }
    }

    /** Where the class library lives once unpacked. */
    fun home(context: Context): File = File(context.filesDir, "jre")

    fun isAvailable(context: Context): Boolean = launcher(context) != null

    /**
     * Unpack the runtime if it has not been unpacked yet.
     *
     * Returns false when no archive is bundled for this device's ABI, which
     * is the honest answer on a build that ships no JRE.
     */
    fun ensureUnpacked(context: Context): Boolean {
        val home = home(context)
        if (File(home, STAMP).exists()) return true

        val abi = android.os.Build.SUPPORTED_ABIS.firstOrNull() ?: return false
        val asset = "$ASSET_PREFIX$abi.zip"

        return try {
            context.assets.open(asset).use { input ->
                home.deleteRecursively()
                home.mkdirs()
                ZipInputStream(input.buffered()).use { zip ->
                    while (true) {
                        val entry = zip.nextEntry ?: break
                        val target = File(home, entry.name)
                        // Zip-slip: an archive entry may contain `..` and land
                        // outside the destination. Refuse rather than trust it.
                        if (!target.canonicalPath.startsWith(home.canonicalPath + File.separator)) {
                            throw SecurityException("archive entry escapes the destination: ${entry.name}")
                        }
                        if (entry.isDirectory) {
                            target.mkdirs()
                        } else {
                            target.parentFile?.mkdirs()
                            target.outputStream().use(zip::copyTo)
                        }
                        zip.closeEntry()
                    }
                }
            }
            File(home, STAMP).writeText(abi)
            Log.i(TAG, "unpacked $asset into $home")
            true
        } catch (missing: java.io.FileNotFoundException) {
            Log.w(TAG, "no JRE bundled for $abi (expected assets/$asset)")
            false
        } catch (err: Exception) {
            Log.e(TAG, "failed to unpack the JRE", err)
            home.deleteRecursively()
            false
        }
    }

    private const val TAG = "HomerunJava"
}
