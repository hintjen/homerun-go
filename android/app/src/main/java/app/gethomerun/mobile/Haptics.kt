package app.gethomerun.mobile

import android.content.Context
import android.media.AudioAttributes
import android.os.Build
import android.os.VibrationAttributes
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.util.Log
import android.view.HapticFeedbackConstants
import android.view.View

private const val TAG = "HomerunHaptics"

/**
 * Plays the `haptic` channel on this device's motor.
 *
 * The page sends what the user just did — not what the motor should do — and
 * the mapping from those six words to Android's own feedback vocabulary lives
 * here. `homerun-app-ui/lib/haptics.ts` (`HAPTIC_MAPPINGS`) is the
 * specification this implements, and `docs/style.md` §16 there says which
 * surfaces are allowed to send which.
 *
 * ## Two paths, and why
 *
 * Four of the six have a `HapticFeedbackConstants` equivalent, and those go
 * through [View.performHapticFeedback]. That is the better path in every way
 * that matters: it needs no permission, it is the same effect the platform's
 * own widgets play, and it honours Settings → Sound & vibration → Touch
 * feedback for free.
 *
 * `success` and `error` have no constant — nothing in that vocabulary means
 * "the thing you asked for finished". They go through [Vibrator], which costs
 * the `VIBRATE` permission and, more importantly, **does not honour the user's
 * touch-feedback setting unless it is told what it is for**. Hence the
 * usage/attributes on every call below: without them this class would buzz a
 * phone whose owner had switched haptics off, which the capability's own
 * contract says a host must not do.
 *
 * ## minSdk 26
 *
 * Most of the constants worth having arrived in API 30 and one in 34, so every
 * branch below falls back. This matters more than it looks: `compileSdk` is 35
 * so the newer symbols resolve at compile time, and `abortOnError = false` in
 * the app's lint config means a missing guard is **not** a build failure — it
 * is a `NoSuchFieldError` on a real API-30 phone. The guards are hand-written
 * because nothing here will write them for you.
 *
 * No debouncing: the UI already rate-limits to one haptic per 50 ms before it
 * sends (`lib/haptics.ts`, `MIN_INTERVAL_MS`).
 */
class Haptics(context: Context) {

    private val vibrator: Vibrator? = resolveVibrator(context)

    /**
     * Play [pattern], or log why nothing happened.
     *
     * Main thread only — [View.performHapticFeedback] touches the view. The
     * caller passes the WebView, which may be null while the render process is
     * being rebuilt.
     */
    fun play(pattern: String, view: View?) {
        when (pattern) {
            // The value under the finger changed. SEGMENT_TICK is the one the
            // platform's own pickers use; CLOCK_TICK is its ancestor and the
            // closest thing below API 34.
            "selection" -> feedback(
                view,
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    HapticFeedbackConstants.SEGMENT_TICK
                } else {
                    HapticFeedbackConstants.CLOCK_TICK
                },
                pattern,
            )

            // A gesture or navigation landed. GESTURE_END is literally that —
            // it is what the system plays when a back gesture commits.
            "navigate" -> feedback(
                view,
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    HapticFeedbackConstants.GESTURE_END
                } else {
                    HapticFeedbackConstants.VIRTUAL_KEY
                },
                pattern,
            )

            // A consequential action was authorised. CONFIRM is the weightier
            // of the platform's two verdict effects.
            "commit" -> if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                feedback(view, HapticFeedbackConstants.CONFIRM, pattern)
            } else {
                heavyClick(pattern)
            }

            // The input was refused before anything was attempted. REJECT is
            // the other verdict effect, and it is unmistakably a "no".
            "warning" -> if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                feedback(view, HapticFeedbackConstants.REJECT, pattern)
            } else {
                heavyClick(pattern)
            }

            // The thing the user asked for finished. Two clicks, because one
            // is indistinguishable from a selection and this has to read as an
            // outcome rather than an acknowledgement.
            "success" -> success(pattern)

            // It was attempted and it failed. A two-pulse waveform: longer and
            // rougher than anything above, so it is felt as an interruption.
            "error" -> vibrate(
                VibrationEffect.createWaveform(longArrayOf(0, 40, 60, 40), -1),
                pattern,
            )

            // Ignored rather than an error, per the channel contract: v1 is
            // additive, so a pattern added later must degrade to silence on a
            // host that predates it rather than blow up.
            else -> Log.w(TAG, "unknown pattern \"$pattern\", ignored")
        }
    }

    /**
     * The preferred path. Returns false — and plays nothing — when the user has
     * touch feedback switched off, which is why the log says which happened.
     */
    private fun feedback(view: View?, constant: Int, pattern: String) {
        if (view == null) {
            Log.w(TAG, "$pattern: no WebView attached, nothing played")
            return
        }
        val played = view.performHapticFeedback(constant)
        Log.i(TAG, "$pattern: performHapticFeedback($constant) -> $played")
    }

    /** `success`, best effort down the API ladder. */
    private fun success(pattern: String) {
        val effect = when {
            // Two sharp clicks 80 ms apart. The composition API is the only
            // one that can place them precisely.
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.R ->
                VibrationEffect.startComposition()
                    .addPrimitive(VibrationEffect.Composition.PRIMITIVE_CLICK)
                    .addPrimitive(VibrationEffect.Composition.PRIMITIVE_CLICK, 1f, 80)
                    .compose()

            Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q ->
                VibrationEffect.createPredefined(VibrationEffect.EFFECT_DOUBLE_CLICK)

            // API 26 has neither, so spell the two pulses out by hand.
            else -> VibrationEffect.createWaveform(longArrayOf(0, 30, 80, 30), -1)
        }
        vibrate(effect, pattern)
    }

    /** The pre-API-30 stand-in for CONFIRM and REJECT. */
    private fun heavyClick(pattern: String) {
        val effect = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            VibrationEffect.createPredefined(VibrationEffect.EFFECT_HEAVY_CLICK)
        } else {
            VibrationEffect.createOneShot(40, VibrationEffect.DEFAULT_AMPLITUDE)
        }
        vibrate(effect, pattern)
    }

    /**
     * The [Vibrator] path, always declaring what it is for.
     *
     * The usage is what makes the OS apply the user's haptics setting and
     * intensity to this. An untagged `vibrate(effect)` is treated as an alarm
     * or an unknown, and buzzes regardless — which is the bug this argument
     * exists to prevent, and it is invisible on a device whose owner has never
     * turned haptics off.
     */
    @Suppress("DEPRECATION")
    private fun vibrate(effect: VibrationEffect, pattern: String) {
        val device = vibrator
        if (device == null || !device.hasVibrator()) {
            Log.i(TAG, "$pattern: no vibrator on this device")
            return
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            device.vibrate(effect, VibrationAttributes.createForUsage(VibrationAttributes.USAGE_TOUCH))
        } else {
            device.vibrate(
                effect,
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_ASSISTANCE_SONIFICATION)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                    .build(),
            )
        }
        Log.i(TAG, "$pattern: VibrationEffect")
    }

    private companion object {
        @Suppress("DEPRECATION")
        fun resolveVibrator(context: Context): Vibrator? =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                (context.getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as? VibratorManager)
                    ?.defaultVibrator
            } else {
                context.getSystemService(Context.VIBRATOR_SERVICE) as? Vibrator
            }
    }
}
