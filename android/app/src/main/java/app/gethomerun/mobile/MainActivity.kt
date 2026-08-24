package app.gethomerun.mobile

import android.Manifest
import android.annotation.SuppressLint
import android.content.Intent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.ViewGroup
import android.net.Uri
import android.webkit.ConsoleMessage
import android.webkit.JavascriptInterface
import android.webkit.RenderProcessGoneDetail
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.widget.FrameLayout
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.ActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import androidx.core.graphics.ColorUtils
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.lifecycleScope
import androidx.webkit.WebViewAssetLoader
import androidx.webkit.WebViewClientCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import java.util.Locale

/**
 * The whole app shell: one WebView running the shared UI, one bridge router
 * behind it.
 *
 * The activity owns the WebView's lifecycle; the router deliberately does not,
 * because the render process can die independently and the router must survive
 * to re-arm itself (PROTOCOL.md §4.3).
 */
class MainActivity : ComponentActivity() {

    private lateinit var container: FrameLayout
    private lateinit var assetLoader: WebViewAssetLoader
    private lateinit var router: BridgeRouter
    private var webView: WebView? = null

    /** Debug builds only; see [installDebugJsErrorTriggers]. */
    private var debugJsErrorReceiver: BroadcastReceiver? = null

    /**
     * The space the system bars occupy; zero until the first inset pass.
     *
     * Written on the UI thread and read from a binder thread by
     * `ChromeInterface.safeArea`, which is why it is volatile.
     */
    @Volatile
    private var bars: Insets = Insets.NONE

    /**
     * How much of the window the soft keyboard covers; zero when it is down.
     *
     * Same threading as [bars], and read for the same reason: while the
     * keyboard is up it is over the navigation bar, so the page must stop
     * holding space for a bar nobody can see.
     */
    @Volatile
    private var keyboard: Int = 0

    /**
     * The file the page's `<input type="file">` is waiting on.
     *
     * Held across the activity result because a `WebChromeClient` callback
     * cannot be handed to `startActivityForResult` directly. **It must be
     * answered exactly once, on every path** — a `filePathCallback` that is
     * dropped leaves the input permanently dead: the WebView believes a chooser
     * is still open and refuses to raise another for the life of the page, so
     * the second tap does nothing and there is nothing in the log.
     */
    private var pendingFileChooser: ValueCallback<Array<Uri>>? = null

    /** Answers [pendingFileChooser] and clears it, whatever the outcome was. */
    private fun settleFileChooser(result: ActivityResult?) {
        val callback = pendingFileChooser ?: return
        pendingFileChooser = null
        // `parseResult` returns null for a cancelled chooser, which is the
        // value the WebView needs to unstick the input — not an empty array,
        // and not nothing at all.
        val uris = result?.let {
            WebChromeClient.FileChooserParams.parseResult(it.resultCode, it.data)
        }
        callback.onReceiveValue(uris)
    }

    private val chooseFile =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            settleFileChooser(result)
        }

    private val requestNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            // Hosting is unaffected either way — this is the notification, not
            // the service. Logged because a denied prompt is the explanation
            // for a hosting session with no visible indicator, and nothing else
            // would say so.
            Log.i(TAG, if (granted) "notifications allowed" else "notifications denied; hosting silently")
            // The service is already foreground by now and its notification was
            // posted into a void. It has to enter the foreground again to
            // become visible — see ServerHost.refreshHosting.
            if (granted) ServerHost.refreshHosting()
            // The bridge's `push:request-permission` may be suspended on this
            // very sheet; answer it in the contract's vocabulary.
            pendingPushPermission.getAndSet(null)?.complete(if (granted) "granted" else "denied")
        }

    /** A `push:request-permission` waiting for the sheet above to be answered. */
    private val pendingPushPermission =
        java.util.concurrent.atomic.AtomicReference<kotlinx.coroutines.CompletableDeferred<String>?>(null)

    /**
     * Asks for POST_NOTIFICATIONS the first time this process hosts anything.
     *
     * Deliberately not at launch. The permission buys one thing — the hosting
     * notification being *visible*, with its Stop action — and asking for it
     * against a dashboard the user has just opened is a prompt with no
     * apparent reason. Asking as a server starts is the same prompt with an
     * obvious one, and the activity is by definition in front of the user at
     * that moment because they just tapped Start.
     *
     * It gates nothing: denied, the foreground service still runs and the
     * server still hosts. See AndroidManifest.xml.
     */
    private val hostingListener = object : ServerHost.Listener {
        override fun onStateChanged(serverId: String, state: ServerState, backupInProgress: Boolean) {
            if (state == ServerState.STARTING) {
                runOnUiThread { askForNotifications() }
                return
            }
            // A run ending is one of the two moments this device stops being
            // busy, which is what holds a staged bundle back — see
            // [applyStagedBundle]. A no-op unless one is waiting.
            runOnUiThread { applyStagedBundle("the server reached $state") }
        }

        /** The other moment: the world has finished going up. */
        override fun onBackupFinished(serverId: String) {
            runOnUiThread { applyStagedBundle("the backup finished") }
        }
    }

    /**
     * Back, while the page has somewhere to go back *to* — and only while.
     *
     * An always-enabled callback is the trap, and it hides well: androidx routes
     * it to an `OnBackInvokedCallback` under the manifest's
     * `enableOnBackInvokedCallback`, so back keeps working and nothing looks
     * wrong. But an enabled callback is this app promising the system it will
     * handle the gesture, so the system cannot render the predictive
     * back-to-home animation — the player holds the swipe and sees nothing move
     * behind the app. The `finish()` this used to call at the root of the
     * history skipped the exit transition for the same reason. Disabled, the
     * dispatcher falls through to the platform and both come back. The app is
     * on targetSdk 36, where this is the platform's default rather than
     * something the manifest asks for, so there is no longer a version of this
     * app in which getting it wrong would go unnoticed.
     *
     * Registered against the *activity*, not a WebView: the render process can
     * die and be rebuilt underneath this ([installWebView]), and the enabled
     * state has to be re-answered when it is — see [syncBackCallback].
     */
    private val backToPreviousPage = object : OnBackPressedCallback(false) {
        override fun handleOnBackPressed() {
            val view = webView
            if (view != null && view.canGoBack()) {
                view.goBack()
                return
            }
            // Raced: the history emptied between the last sync and this press.
            // Hand the press back rather than dropping it — disabled, this
            // second dispatch reaches the platform's own default, which is the
            // finish this used to do by hand, with the transition it never had.
            isEnabled = false
            onBackPressedDispatcher.onBackPressed()
        }
    }

    /**
     * Re-answer "is there anywhere to go back to".
     *
     * From [BundleClient.doUpdateVisitedHistory], because that is what the
     * WebView calls both for a real navigation and for the shared UI's own
     * `pushState` routing — `onPageFinished` fires once for the whole SPA and
     * would never fire again, so the callback would be stuck on the answer the
     * first screen gave.
     *
     * And from [installWebView], which is the sharper one: a rebuilt WebView
     * starts with an empty history, and a callback left enabled over it would
     * swallow every back press into a `goBack()` that does nothing. A dead back
     * button, with nothing in the log to say why.
     */
    private fun syncBackCallback() {
        backToPreviousPage.isEnabled = webView?.canGoBack() == true
    }

    /**
     * Injected before any page script runs. Two jobs:
     *
     *  - `__homerunCapabilities`, which the UI reads **synchronously** at
     *    startup and cannot await.
     *  - `__homerunHostRevision`, for the same reason it has to be synchronous:
     *    a bundle delivered over the air can be newer than the host under it,
     *    and a feature gated on a channel this host does not answer must render
     *    as absent rather than as a button that hangs. A sibling global rather
     *    than a capability field, because capabilities are generated from the
     *    contract and this is a property of the binary.
     *  - `__homerunHost.postMessage`, the name the shared transport looks for.
     *    `addJavascriptInterface` gives us `HomerunHost`; this is the adapter
     *    between that and the protocol's global.
     */
    private val bootstrapScript: String by lazy {
        val capabilities = Json.encodeToString(HostCapabilities.ANDROID)
        """
        (function () {
          window.__homerunCapabilities = $capabilities;
          window.__homerunHostRevision = ${BridgeRouter.HOST_REVISION};
          var host = window.__homerunHost || (window.__homerunHost = {});
          host.postMessage = function (json) { ${BridgeRouter.JS_INTERFACE}.postMessage(json); };

          // Uncaught page errors, for the window before the bundle can report
          // its own. A bundle that throws on its way up leaves a blank screen
          // with no page left to report from, and that is the one failure a
          // React error boundary can never see — the boundary is inside the
          // tree that never mounted.
          //
          // It stands down the moment the page's own reporter is live. The
          // shared UI installs listeners with a real stack and a real error
          // name as soon as _app.tsx evaluates; without the check below every
          // UI error after boot would arrive twice, and the second copy is
          // strictly the worse one — window.onerror gives a file and a line
          // and nothing else.
          //
          // Kept identical to BridgeController.errorHookScript() on iOS. The
          // two hosts inject the same behaviour and neither has anywhere
          // shared to put JavaScript.
          function preBootError(message, source, line) {
            if (window.__homerunPageErrors) return;
            try {
              host.postMessage(JSON.stringify({
                v: 1, method: '__host:jsError',
                params: { message: String(message), source: String(source), line: line }
              }));
            } catch (e) {}
          }
          window.addEventListener('error', function (e) {
            preBootError(e.message, e.filename, e.lineno);
          });
          window.addEventListener('unhandledrejection', function (e) {
            preBootError('Unhandled rejection: ' + String(e.reason), '', 0);
          });

          var root = document.documentElement;

          // The system bars' size, in the variables the shared UI already
          // reads. It defines them from `env(safe-area-inset-*)`, which is
          // right on iOS and always zero here — Android WebView fills those in
          // from a display cutout and never from the bars. An inline style on
          // <html> outranks the :root rule, so this is a substitution rather
          // than a fork: every `pt-safe`, `pb-safe` and `px-safe` in the bundle
          // starts working, and the page holds its own content clear of the
          // clock exactly as it does on the other platform.
          //
          // Read back synchronously rather than pushed, because a document
          // that has just started parsing has no styles of ours on it yet and
          // must not paint even once without them.
          window.__homerunSafeArea = function () {
            if (!root) return;
            try {
              var v = ${CHROME_INTERFACE}.safeArea().split(' ');
              root.style.setProperty('--safe-top', v[0]);
              root.style.setProperty('--safe-right', v[1]);
              root.style.setProperty('--safe-bottom', v[2]);
              root.style.setProperty('--safe-left', v[3]);
            } catch (e) {}
          };
          window.__homerunSafeArea();

          // What the page is painted at its top and bottom edges, reported so
          // the clock and the gesture pill stay legible against it.
          //
          // This is all that is left of the host chasing the page, and it is
          // the part that can afford to: an icon flipping black-on-white a
          // frame late is invisible, where a mismatched band of colour was
          // not.
          //
          // Measured, never inferred, and measured per edge. Two earlier
          // versions of this were wrong in the same way — they answered a
          // question about one thing (which theme is on) when the question is
          // about another (what colour is under the clock *now*):
          //
          //   theme name -> a hex in the host.  The hex was #0A0A0A against the
          //   UI's real #121214. Close enough to look deliberate, far enough to
          //   read as a seam. Nothing can keep those in step: the bundle ships
          //   over the air and the host does not.
          //
          //   body's background.  Right until anything is drawn over it. A
          //   sheet dims the page behind it, so the clock kept its undimmed
          //   appearance over a dimmed screen.
          //
          // So: ask what is actually on screen at each edge. elementFromPoint
          // gives the topmost element there, its ancestors are what is painted
          // behind it, and compositing that stack back to front gives the
          // colour an eye would see. A dim overlay contributes its alpha and
          // the page contributes what shows through. Per edge, because a sheet
          // that reaches the bottom of the screen but not the top leaves the
          // two ends of the screen genuinely different colours.
          var query = window.matchMedia('(prefers-color-scheme: dark)');
          var last = '';

          function parse(css, opacity) {
            var m = /rgba?\(([^)]+)\)/.exec(css || '');
            if (!m) return null;
            var p = m[1].split(',').map(parseFloat);
            var a = (p.length > 3 ? p[3] : 1) * opacity;
            return a > 0 ? { r: p[0], g: p[1], b: p[2], a: a } : null;
          }

          function edge(y) {
            var el = document.elementFromPoint(Math.floor(window.innerWidth / 2), y);
            var stack = [];
            while (el) {
              var style = getComputedStyle(el);
              // An ancestor's opacity dims everything already collected from
              // inside it, its own background included. Framer Motion fades
              // overlays in exactly this way, so without it the answer would
              // jump to the settled colour on the first frame of the fade.
              var o = parseFloat(style.opacity);
              if (o < 1) for (var j = 0; j < stack.length; j++) stack[j].a *= o;
              var c = parse(style.backgroundColor, isNaN(o) ? 1 : o);
              if (c) { stack.push(c); if (c.a >= 1) break; }
              el = el.parentElement;
            }
            // Nothing opaque underneath means there is nothing to report yet —
            // at document start there is no stylesheet and usually no <body>.
            // Saying nothing leaves the host on its launch blue, which is what
            // belongs on screen at that point anyway.
            var out = stack.pop();
            if (!out || out.a < 1) return '';
            while (stack.length) {
              var s = stack.pop();
              out = {
                r: s.r * s.a + out.r * (1 - s.a),
                g: s.g * s.a + out.g * (1 - s.a),
                b: s.b * s.a + out.b * (1 - s.a),
                a: 1,
              };
            }
            return 'rgb(' + Math.round(out.r) + ',' + Math.round(out.g) + ',' +
                   Math.round(out.b) + ')';
          }

          function report() {
            var top = edge(1);
            var bottom = edge(Math.max(1, window.innerHeight - 2));
            if (!top || !bottom || top + '|' + bottom === last) return;
            last = top + '|' + bottom;
            try { ${CHROME_INTERFACE}.backdropChanged(top, bottom); } catch (e) {}
          }

          // Sampled for a short while after anything moves, rather than once:
          // a sheet slides and a dim fades, and the answer during the animation
          // is different from the answer after it.
          //
          // Every frame rather than on a timer, so the clock flips in the
          // middle of the fade that makes it necessary instead of a beat after
          // it. The burst is bounded and `report` does nothing when the
          // composite has not moved, so this costs close to nothing.
          var deadline = 0;
          var running = false;
          function frame() {
            report();
            if (Date.now() < deadline) requestAnimationFrame(frame);
            else running = false;
          }
          function schedule() {
            deadline = Date.now() + 500;
            report();
            if (!running) { running = true; requestAnimationFrame(frame); }
          }

          // Deliberately not a subtree observer. Overlays and sheets portal
          // into <body> as direct children, and a page whose console is
          // streaming a server's output mutates deep in the tree hundreds of
          // times a second — watching all of it would run this sampler forever
          // for nothing. The theme class lands on <html>; scroll covers a
          // header that changes colour as it passes under the clock.
          new MutationObserver(schedule).observe(root, {
            attributes: true, attributeFilter: ['class'],
          });
          function watchBody() {
            if (document.body) {
              new MutationObserver(schedule).observe(document.body, {
                attributes: true, childList: true,
              });
            }
          }
          document.addEventListener('DOMContentLoaded', function () {
            watchBody();
            schedule();
          });
          // The media listener covers the device's appearance changing under a
          // `system` page, which the activity does not otherwise notice: uiMode
          // is in configChanges, so nothing is recreated and the theme XML is
          // never re-applied.
          query.addEventListener('change', schedule);
          window.addEventListener('scroll', schedule, true);
          window.addEventListener('resize', schedule);
          watchBody();
          schedule();
        })();
        """.trimIndent()
    }

    /**
     * The page telling the host what it looks like. Called on a binder thread,
     * like every `@JavascriptInterface` method.
     *
     * Deliberately not a bridge channel: this is host chrome, not part of the
     * `bridge/v1` contract, and putting it through the router would put a
     * method in the dispatch table that no manifest declares.
     */
    private inner class ChromeInterface {
        @JavascriptInterface
        fun backdropChanged(top: String, bottom: String) {
            val above = parseCssColour(top) ?: return
            val below = parseCssColour(bottom) ?: return
            runOnUiThread { applyChrome(above, below) }
        }

        /**
         * `top right bottom left`, in CSS pixels — the order the shorthand
         * everyone already knows uses, so the JS side can split and assign.
         *
         * CSS pixels, not device pixels: the bundle sets `initial-scale=1` on
         * a `device-width` viewport, so one CSS pixel is one dp. Fractional on
         * purpose. Rounding a 21.33dp gesture inset to 21 leaves a sliver of
         * page under the pill on exactly the phones where it is tightest.
         *
         * The keyboard comes off the bottom, because the WebView has already
         * been shortened to sit on top of it and the navigation bar it would
         * otherwise be avoiding is *behind* the keys. Left in, the page would
         * hold 24dp clear of a pill it cannot reach and every sheet would float
         * that far above the keyboard. Clamped rather than assumed one-sided: a
         * floating or split keyboard reports less than the bar, and the bar is
         * then still there to avoid.
         */
        @JavascriptInterface
        fun safeArea(): String {
            val density = resources.displayMetrics.density
            val bars = bars
            val bottom = (bars.bottom - keyboard).coerceAtLeast(0)
            // Locale.US, not the default: a device set to a comma-decimal
            // locale would format 21.33 as "21,33px", which is not a CSS
            // length. The page would silently keep its zeroes and draw under
            // the bars in exactly the places this exists to prevent.
            return "%.2fpx %.2fpx %.2fpx %.2fpx".format(
                Locale.US,
                bars.top / density,
                bars.right / density,
                bottom / density,
                bars.left / density,
            )
        }
    }

    /**
     * `rgb(18, 18, 20)` — what the page's compositing produced. Alpha never
     * arrives here: the page has already flattened its stack against something
     * opaque, and a colour that could not be flattened is not sent at all.
     *
     * Returns null rather than a guess. The caller's job is then to leave the
     * chrome exactly as it was, which beats flashing a wrong colour.
     */
    private fun parseCssColour(css: String): Int? {
        val channels = CSS_CHANNEL.findAll(css)
            .take(3)
            .mapNotNull { it.value.toIntOrNull()?.takeIf { n -> n in 0..255 } }
            .toList()
        if (channels.size < 3) {
            Log.w(TAG, "unparseable backdrop colour: $css")
            return null
        }
        return Color.rgb(channels[0], channels[1], channels[2])
    }

    /**
     * Back to what the theme starts on: blue everywhere, white icons. Used
     * whenever there is no page — cold start, and again after the render
     * process dies and the WebView is rebuilt.
     */
    private fun applyLaunchChrome() =
        ContextCompat.getColor(this, R.color.launch_background)
            .let { applyChrome(it, it) }

    /**
     * Sets the clock and the gesture pill to whatever stays legible against
     * what the page is painting behind them.
     *
     * Per bar, because the two ends of the screen are not always the same: a
     * sheet covers the bottom and dims the top, and one appearance for both
     * puts a black clock on a dimmed page.
     *
     * By luminance rather than by theme. Brand blue is not a dark colour in
     * the sense a theme means, and still needs a white clock on it.
     *
     * The window bar colours are set too and matter only on API 34 and below;
     * from 35 the platform ignores them and shows the page through the bar.
     * The container is behind the WebView, so it only shows in the moment
     * before the page's first paint.
     */
    private fun applyChrome(top: Int, bottom: Int) {
        container.setBackgroundColor(top)
        @Suppress("DEPRECATION")
        window.statusBarColor = top
        @Suppress("DEPRECATION")
        window.navigationBarColor = bottom
        WindowInsetsControllerCompat(window, window.decorView).apply {
            isAppearanceLightStatusBars = ColorUtils.calculateLuminance(top) > 0.5
            isAppearanceLightNavigationBars = ColorUtils.calculateLuminance(bottom) > 0.5
        }
    }

    /**
     * Keeps the page out from under the status bar and the gesture pill.
     *
     * From Android 15 an app targeting SDK 35 is edge-to-edge and cannot opt
     * out: the window is the whole display, and a WebView filling it draws
     * beneath the clock at the top and beneath the navigation pill at the
     * bottom. On this phone that put the "Homerun Go" wordmark behind the
     * status icons and the pill straight through the "Create" tab label.
     *
     * iOS has never had this problem, because WKWebView answers
     * `env(safe-area-inset-*)` and the shared UI — which does set
     * `viewport-fit=cover` and does consume those variables — gets real numbers
     * back. Android WebView only ever fills them in from a display *cutout*,
     * never from the bars, so on most phones the UI is asking a question the
     * platform will not answer.
     *
     * So the host measures the bars and hands the page the numbers, in the
     * variables the shared UI already reads. The WebView stays full-bleed and
     * the *page* keeps its own content clear of them, which is exactly what it
     * does on iOS — the only difference being where the numbers came from.
     *
     * **The host must not hold the space itself.** It was tried: inset the
     * WebView, fill the gap with two views, colour them from what the page
     * reported. It works until something animates. The page's dim and the
     * host's strips are painted by two different compositors, so the strips
     * always arrive a frame late and the seam is visible every time a sheet
     * opens. Nothing about the sampling rate fixes that — the fix is for the
     * bars to be part of the page, so there is one thing painting and nothing
     * to keep in step.
     *
     * `CONSUMED` stops the dispatch here so the WebView never sees a display
     * cutout of its own. `env(safe-area-inset-*)` therefore stays 0 and the
     * variables below are the single source of the numbers.
     *
     * ## The keyboard is part of this, and used not to be
     *
     * `adjustResize` in the manifest does **not** shrink the window, because
     * this app targets SDK 35 and is therefore edge-to-edge: the platform only
     * insets a window's content while it fits system windows, and an
     * edge-to-edge window by definition does not. An earlier version of this
     * comment asserted the opposite and excluded `Type.ime()` to avoid
     * double-counting a resize that never happens. Nothing counted it at all,
     * and the symptom was precise: open the claim-account sheet, the keyboard
     * comes up over it, and the field being typed into is behind the keys.
     * `innerHeight` stayed at its full 997 with `mInputShown=true`.
     *
     * So the host resizes the WebView by hand, which is the one place the
     * numbers exist — a page cannot see an Android keyboard.
     *
     * Padding on the container rather than a report to the page, because a real
     * resize is what the shared UI already asks for: its viewport tag carries
     * `interactive-widget=resizes-content`, whose entire job is to shrink the
     * layout viewport for the keyboard rather than pan it, and which is inert
     * while the window it sits in never changes size. Honour that and `100dvh`,
     * `bottom-0`, `--visual-vh` and `.pb-keyboard` all come right at once, for
     * every sheet in the bundle, with nothing to keep in step. Reporting a
     * height instead would mean teaching ~40 call sites about a variable one
     * platform sets, to reach the same place.
     *
     * `adjustResize` stays in the manifest even though it is inert. Dropping it
     * leaves the mode unspecified, and the platform may then pick `adjustPan` —
     * which would slide the whole page up, status bar and all.
     *
     * The strip left below the WebView is exactly where the keyboard is, so it
     * is only visible for the frames of the open animation. That is the same
     * trade `adjustResize` itself made.
     */
    private fun holdSystemUiOutOfThePage() {
        ViewCompat.setOnApplyWindowInsetsListener(container) { _, insets ->
            bars = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout(),
            )
            val ime = insets.getInsets(WindowInsetsCompat.Type.ime()).bottom
            if (ime != keyboard) {
                keyboard = ime
                // Children are MATCH_PARENT, so this is the WebView's height.
                container.setPadding(0, 0, 0, ime)
            }
            // A page already loaded needs telling; one loading now reads the
            // same numbers itself, from the document-start script.
            webView?.evaluateJavascript("window.__homerunSafeArea && __homerunSafeArea()", null)
            WindowInsetsCompat.CONSUMED
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Before anything can load a page. A bundle downloaded on an earlier
        // launch goes live here and nowhere else — never under a live WebView,
        // which would cancel whatever bridge call is in flight, and
        // `native-server-start` runs for minutes (plans/ota-updates.md).
        BundleStore.activate(this)

        router = BridgeRouter(applicationContext, lifecycleScope)

        // Remote push: the router needs this activity for the permission
        // sheet, and the FCM service needs the router for token rotations.
        // Both references die with the activity (onDestroy), like every
        // other router tie.
        PushMessaging.router = router
        router.requestPushPermission = requestPushPermission@{
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
                // No runtime permission to ask for; the state is the answer.
                return@requestPushPermission router.pushPermissionStatus()
            }
            if (router.pushPermissionStatus() != "notDetermined") {
                // iOS cannot re-prompt after a denial and neither can
                // Android 13+ (the sheet is silently swallowed) — resolving
                // immediately with the truth beats hanging on a sheet that
                // will never appear.
                return@requestPushPermission router.pushPermissionStatus()
            }
            val deferred = kotlinx.coroutines.CompletableDeferred<String>()
            pendingPushPermission.set(deferred)
            // Either prompt marks "asked": it is the same OS permission.
            getSharedPreferences("homerun-host", MODE_PRIVATE)
                .edit().putBoolean(BridgeRouter.KEY_PUSH_ASKED, true).apply()
            runCatching { requestNotifications.launch(Manifest.permission.POST_NOTIFICATIONS) }
                .onFailure {
                    pendingPushPermission.set(null)
                    deferred.complete(router.pushPermissionStatus())
                }
            deferred.await()
        }

        // A tap on a remote notification cold-starts the activity with the
        // message's data payload in the launcher intent. Queued through the
        // router (`push:opened` rides the ready handshake), because the page
        // does not exist yet — the same shape as the cold-start deep link
        // below.
        intent?.let { deliverPushTap(it) }

        container = FrameLayout(this).apply {
            layoutParams = ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            )
        }
        setContentView(container)
        holdSystemUiOutOfThePage()

        // Before the page loads, so `deep-link:consume` finds it on mount.
        intent?.dataString?.let {
            Log.i(TAG, "cold-start deep link: $it")
            router.captureColdStartDeepLink(it)
        }

        installWebView()

        // After the page has been asked to load, never before. This is seconds
        // of network and disk for something that takes effect on the *next*
        // launch, so there is nothing to gain by making the user wait on it.
        BundleUpdater.check(this, lifecycleScope)

        // Apply it as soon as it is on disk, rather than asking. There is no
        // update prompt on this host: `update-available` is what the shared
        // UI's card subscribes to, and never emitting it is what keeps the
        // card off the screen.
        BundleUpdater.onBundleStaged = { bundle ->
            Log.i(TAG, "bundle $bundle is staged")
            runOnUiThread { applyStagedBundle("it was just staged") }
        }

        // `quit-and-install`, which on this platform installs without quitting.
        // Promoting is safe here for the same reason it is safe in onCreate —
        // a page is about to be built either way, so nothing is swapped under a
        // live one and no bridge call is cancelled mid-flight.
        router.onAppearance = { appearance ->
            runOnUiThread {
                // A light page wants dark glyphs, and the reverse. The colour
                // path in applyChrome computes this from luminance; this is the
                // page saying it outright, which is all we get before it has
                // sent any colours.
                WindowInsetsControllerCompat(window, window.decorView).apply {
                    isAppearanceLightStatusBars = appearance == "light"
                    isAppearanceLightNavigationBars = appearance == "light"
                }
            }
        }

        router.onApplyUpdate = {
            runOnUiThread {
                if (!isFinishing && !isDestroyed) {
                    Log.i(TAG, "applying a staged bundle at the page's request")
                    BundleStore.activate(this)
                    installWebView()
                }
            }
        }

        // The other half of applying immediately: a bundle that arrived while
        // the page was mid-call goes live the moment that call is done, rather
        // than waiting for the next launch.
        router.onPageIdle = { applyStagedBundle("the page went idle") }

        ServerHost.addListener(hostingListener)

        // Debug builds only. See [installDebugJsErrorTriggers].
        installDebugJsErrorTriggers()

        onBackPressedDispatcher.addCallback(this, backToPreviousPage)
    }

    /**
     * Make the page fail on purpose, to prove the document-start hook works
     * and then to prove it gets out of the way. **Debug builds only.**
     *
     *     adb shell am broadcast -a app.gethomerun.mobile.DEBUG_JS_ERROR
     *     adb shell am broadcast -a app.gethomerun.mobile.DEBUG_JS_ERROR --es mode handoff
     *
     * The default `preboot` mode clears the flag the bundle sets when its own
     * reporter comes up, putting the page back in the state it is in while it
     * is still loading, and then throws. That must produce a row with
     * `kind: "boot"` — the failure a React error boundary can never see,
     * because the tree it would live in never mounted.
     *
     * `handoff` mode sets the flag and throws. That must produce
     * exactly one row, from the bundle's own listener, with a real error name
     * and a real stack — and no `boot` row beside it. Both halves need
     * checking: a hook that never fires and a hook that fires twice are
     * different bugs with the same cause, and only the second one is visible
     * in a query that is not looking for it.
     *
     * `setTimeout` rather than a bare throw, because an exception raised
     * directly inside `evaluateJavascript` is caught by the evaluation itself
     * and never reaches `window.onerror`. A task scheduled onto the event loop
     * fails the way a real uncaught error does.
     */
    private fun installDebugJsErrorTriggers() {
        if (!BuildConfig.DEBUG) return

        val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                val handoff = intent?.getStringExtra("mode") == "handoff"
                // Each mode states the flag rather than assuming it. `handoff`
                // used to merely refrain from clearing it, which meant a
                // `preboot` run left it false and the next `handoff` silently
                // became a second `preboot` — the one outcome that makes the
                // test look like it passed while proving nothing.
                val clear = "window.__homerunPageErrors = $handoff;"
                val what = if (handoff) "handoff" else "pre-boot"
                Log.i(TAG, "debug: forcing a $what JS error")
                webView?.evaluateJavascript(
                    "$clear setTimeout(function () {" +
                        " throw new Error('deliberate $what JS error, for verification');" +
                        " }, 0);",
                    null,
                )
            }
        }

        val filter = IntentFilter(DEBUG_JS_ERROR_ACTION)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            registerReceiver(receiver, filter)
        }
        debugJsErrorReceiver = receiver
        Log.i(TAG, "debug: JS error triggers listening on $DEBUG_JS_ERROR_ACTION")
    }

    /**
     * Put a downloaded bundle on screen, if this is a safe moment to.
     *
     * There is no update prompt on this host. A bundle that has been fetched,
     * verified and unpacked goes live as soon as the app can take it, which is
     * usually within a second of it arriving — the user sees a splash and the
     * app comes back on the new UI. The alternative was a card asking
     * permission for something that costs a second and that nobody can
     * evaluate, and "later" meant the next launch anyway.
     *
     * **Two things make now the wrong moment**, and both defer rather than
     * cancel:
     *
     *  - **A bridge call is in flight.** Rebuilding the WebView cancels every
     *    handler the page owns ([BridgeRouter.onPageGone]). `wait-for-update-check`
     *    is itself one of them — it is awaited on the mandatory post-login
     *    path, so applying underneath it would hang login at a spinner, which
     *    is the exact failure this repo is most careful about.
     *  - **This device is hosting.** A running server survives the swap —
     *    that is what `ServerHost` is for — but the console scrollback does
     *    not, and interrupting someone mid-session to reload the UI is a poor
     *    trade for a fix that can wait for the stop. `busy` also covers the
     *    on-stop backup, which runs for minutes after the server has gone.
     *
     * Every path back to idle calls this again: the router when the last
     * handler unwinds, the hosting listener when a run and its backup end,
     * [onResume] when the user comes back. And if none of them ever does,
     * `BundleStore.activate` at the next launch still picks it up — that path
     * is untouched and remains the floor.
     */
    private fun applyStagedBundle(trigger: String) {
        if (isFinishing || isDestroyed) return
        val staged = runCatching { BundleStore.pending(this) }.getOrNull() ?: return

        if (!router.pageIdle()) {
            Log.i(TAG, "holding $staged back ($trigger): the page is mid-call")
            return
        }
        if (ServerHost.hosting().busy) {
            Log.i(TAG, "holding $staged back ($trigger): ${ServerHost.hostingSummary()}")
            return
        }

        Log.i(TAG, "applying $staged now ($trigger)")
        BundleStore.activate(this)
        installWebView()
    }

    /** Builds a fresh WebView, wires it to the router, and loads the bundle. */
    @SuppressLint("SetJavaScriptEnabled")
    private fun installWebView() {
        webView?.let { old ->
            container.removeView(old)
            old.destroy()
        }

        // Re-asked for every WebView, not just the first. A render process that
        // died is the strongest evidence a bundle is bad, and asking again is
        // what spends its remaining attempts — so a fatal bundle rolls back
        // within one session instead of waiting for relaunches.
        assetLoader = WebBundle.loader(this, BundleStore.resolve(this).root)

        // No page yet: brand blue behind the bars, white clock on it.
        applyLaunchChrome()

        val view = WebView(this)
        view.layoutParams = FrameLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.MATCH_PARENT,
        )
        // WebView paints white until the document has a background of its own,
        // which is a flash on every launch and every rebuild. This is the same
        // blue as the window behind it and as the bundle's splash.
        view.setBackgroundColor(ContextCompat.getColor(this, R.color.launch_background))

        view.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            mediaPlaybackRequiresUserGesture = false
            // The bundle is served over the asset loader's https origin, so
            // the WebView never needs raw file or content access. Leaving
            // these off closes the classic asset-loader path-traversal hole.
            allowFileAccess = false
            allowContentAccess = false
            // The UI is responsive and sized in CSS pixels; letting the
            // WebView apply its own zoom heuristics fights the layout.
            useWideViewPort = false
            loadWithOverviewMode = false
            setSupportZoom(false)
        }

        view.webViewClient = BundleClient()
        view.webChromeClient = ConsoleClient()
        view.addJavascriptInterface(router, BridgeRouter.JS_INTERFACE)
        view.addJavascriptInterface(ChromeInterface(), CHROME_INTERFACE)

        if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
            WebViewCompat.addDocumentStartJavaScript(view, bootstrapScript, setOf(WebBundle.ORIGIN))
        } else {
            // Fallback for old WebView builds: onPageStarted fires before the
            // page's own scripts, but it is a weaker guarantee than a real
            // document-start script. Logged so a blank screen on an old device
            // has an obvious first suspect.
            Log.w(TAG, "DOCUMENT_START_SCRIPT unsupported; injecting from onPageStarted")
        }

        container.addView(view)
        webView = view
        router.attach(view)
        router.onPageGone()
        // This WebView has no history, whatever the one it replaced had.
        syncBackCallback()

        view.loadUrl(WebBundle.START_URL)
    }

    /**
     * A link arriving while the app is already running. `launchMode` is
     * singleTask, so Android routes it here instead of building a second
     * activity — which is also what keeps the running server's WebView alive.
     */
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        // Keep getIntent() current, or a later re-read returns the stale one.
        setIntent(intent)
        intent.dataString?.let { router.deliverDeepLink(it) }
        deliverPushTap(intent)
    }

    /**
     * Back from the background, where a server may have been running the whole
     * time. The page is told what is true now rather than left with whatever it
     * last heard (PROTOCOL.md §4.3).
     */
    override fun onResume() {
        super.onResume()
        router.capture("host:foregrounded", mapOf(Incidents.hosting(ServerHost.hostingSummary())))
        router.resyncServerState()
        // A dismissed sign-in tab reports nothing, so being visible again with
        // one outstanding is the only evidence the user backed out.
        router.onForegrounded()
        // The other half of "launch and resume". A phone that is never closed
        // would otherwise check once and stay on that bundle for weeks; the
        // throttle inside means most resumes cost nothing.
        BundleUpdater.check(this, lifecycleScope)
        // A bundle staged earlier and held back — the device was hosting, or
        // the page was mid-call — takes the first chance it gets. The check
        // above will not re-announce one it has already staged.
        applyStagedBundle("the app came back to the foreground")
    }

    /**
     * Leaving, with what was running at the time.
     *
     * The pair of this and `host:foregrounded` is the only way to measure the
     * thing this platform actually does to people: backgrounding while hosting
     * is how a session ends on a phone, and nothing in the page can see it
     * happen. The page is still alive here, so this goes over the bridge
     * rather than to disk — unlike the two failures in [Incidents], which kill
     * their own reporter.
     */
    override fun onPause() {
        super.onPause()
        router.capture("host:backgrounded", mapOf(Incidents.hosting(ServerHost.hostingSummary())))
    }

    /**
     * Emit `push:opened` if this intent is a notification tap.
     *
     * FCM stamps `google.message_id` on the launcher intent it fires for a
     * tray tap and copies the message's `data` keys in as extras — that stamp
     * is the discriminator, because everything else about the intent looks
     * like an ordinary launch. `href` is the same string the desktop bell
     * would have opened; the shared UI routes it, not the host.
     */
    private fun deliverPushTap(intent: Intent) {
        if (intent.getStringExtra("google.message_id") == null) return
        val payload = buildJsonObject {
            intent.getStringExtra("href")?.let { put("href", it) }
            intent.getStringExtra("id")?.let { put("id", it) }
        }
        Log.i(TAG, "notification tap: $payload")
        router.emit("push:opened", listOf(payload))
    }

    private fun askForNotifications() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (asked) return
        asked = true
        val granted = ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED
        if (granted) return
        // The bridge's permission vocabulary needs to know a sheet was shown,
        // whichever feature showed it — it is the same OS permission.
        getSharedPreferences("homerun-host", MODE_PRIVATE)
            .edit().putBoolean(BridgeRouter.KEY_PUSH_ASKED, true).apply()
        runCatching { requestNotifications.launch(Manifest.permission.POST_NOTIFICATIONS) }
            .onFailure { Log.w(TAG, "could not ask about notifications: ${it.message}") }
    }

    override fun onDestroy() {
        ServerHost.removeListener(hostingListener)
        // Registered against this activity, so it has to go with it — a
        // receiver outliving its activity is a leaked one, and this one holds
        // a WebView through `webView`.
        debugJsErrorReceiver?.let { runCatching { unregisterReceiver(it) } }
        debugJsErrorReceiver = null
        // The router is built in `onCreate` and subscribes to `ServerHost`,
        // which outlives every activity — so it has to be let go here for the
        // same reason the listener above does. Missing this left one abandoned
        // router per recreation, each still reporting server state to the API.
        // Guarded because `onCreate` can throw before the assignment, and an
        // UninitializedPropertyAccessException here would bury whatever did it.
        PushMessaging.router = null
        if (::router.isInitialized) router.dispose()
        webView?.let {
            container.removeView(it)
            it.destroy()
        }
        webView = null
        super.onDestroy()
    }

    private inner class BundleClient : WebViewClientCompat() {
        override fun shouldInterceptRequest(
            view: WebView,
            request: WebResourceRequest,
        ): WebResourceResponse? = assetLoader.shouldInterceptRequest(request.url)

        override fun shouldOverrideUrlLoading(
            view: WebView,
            request: WebResourceRequest,
        ): Boolean {
            // Anything that is not the bundle is a real link — auth flows,
            // docs, Discord. Those belong in the browser, not in a WebView
            // with a JavaScript bridge attached to it.
            if (request.url.host == WebBundle.DOMAIN) return false
            return runCatching {
                startActivity(
                    Intent(Intent.ACTION_VIEW, request.url)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                )
                true
            }.getOrElse { true }
        }

        /**
         * The only signal that covers the shared UI's client-side routing.
         * Called for a document load and for `pushState`/`replaceState` alike,
         * which in an SPA is nearly every screen change there is.
         */
        override fun doUpdateVisitedHistory(view: WebView, url: String, isReload: Boolean) {
            syncBackCallback()
        }

        override fun onPageStarted(view: WebView, url: String, favicon: android.graphics.Bitmap?) {
            if (!WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
                view.evaluateJavascript(bootstrapScript, null)
            }
        }

        /**
         * Re-reads the safe area, because the document-start read can be stale
         * by now and one of the four numbers moves at runtime.
         *
         * An inset change while a document is loading is pushed with
         * `evaluateJavascript`, which needs `__homerunSafeArea` to already be
         * defined — and it is not, for the window between a document starting
         * and its bootstrap running. Miss the push there and the page keeps the
         * numbers from before it existed until the *next* inset change, with
         * nothing to prompt one.
         *
         * That was survivable while these were only the system bars, which do
         * not move after the first pass. It is not survivable now the keyboard
         * is in them: kill the app with the keyboard up — which is what
         * reinstalling over a running app does — and the page comes back
         * believing a keyboard that is now down is still covering its bottom
         * 24dp. Reading again here costs one JS call per document load.
         */
        override fun onPageFinished(view: WebView, url: String) {
            view.evaluateJavascript("window.__homerunSafeArea && __homerunSafeArea()", null)
        }

        /**
         * The render process was killed — most likely by memory pressure,
         * which is exactly what hosting a Minecraft server on a phone causes.
         * The WebView is unusable from here; returning true means "handled,
         * do not kill the app".
         */
        override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
            Log.w(TAG, "render process gone (crashed=${detail.didCrash()}); rebuilding")
            // To disk, not to the page: the page is what just died, and
            // `onPageGone` is about to clear the queue anything emitted here
            // would sit in. The replacement page reports it at its handshake.
            //
            // `didCrash` was already being read for the log line above and
            // thrown away. It is the difference between the renderer falling
            // over and Android reclaiming it for memory, which is the whole
            // question when the memory pressure is a Minecraft server we
            // started.
            Incidents.record(
                this@MainActivity,
                Incidents.RENDERER_DEATH,
                mapOf(
                    Incidents.didCrash(detail.didCrash()),
                    Incidents.hosting(ServerHost.hostingSummary()),
                )
            )
            router.onPageGone()
            if (!isFinishing && !isDestroyed) installWebView()
            return true
        }
    }

    private inner class ConsoleClient : WebChromeClient() {
        /**
         * Raise the system picker for the page's `<input type="file">`.
         *
         * Without this override a WebView silently does nothing when an input
         * is clicked — the default implementation returns false and drops the
         * callback. There is no error, no console line and no permission
         * prompt; the tap simply has no effect, which is exactly what picking a
         * server icon did on Android.
         *
         * `createIntent()` builds the chooser from the input's own `accept` and
         * `multiple` attributes, so the page keeps deciding what it will take
         * and this stays out of it. It needs no storage permission: the picker
         * runs in its own process and hands back a content URI already granted
         * to us, which is the modern contract and the reason not to hand-roll
         * an `ACTION_GET_CONTENT` here.
         */
        override fun onShowFileChooser(
            webView: WebView?,
            filePathCallback: ValueCallback<Array<Uri>>?,
            fileChooserParams: FileChooserParams?,
        ): Boolean {
            if (filePathCallback == null) return false

            // A chooser already in flight is answered before it is replaced.
            // Two inputs cannot both be waiting, and leaking the first would
            // wedge it for good.
            settleFileChooser(null)
            pendingFileChooser = filePathCallback

            val intent = fileChooserParams?.createIntent()
            if (intent == null) {
                settleFileChooser(null)
                return false
            }

            return try {
                chooseFile.launch(intent)
                true
            } catch (err: Exception) {
                // No app on the device can satisfy the intent. Answering the
                // callback is what lets the input be tapped again.
                Log.w(TAG, "no picker for the file input: ${err.message}")
                settleFileChooser(null)
                false
            }
        }

        override fun onConsoleMessage(message: ConsoleMessage): Boolean {
            // Without this the shared UI's errors are invisible: they go to a
            // console nothing is attached to, and the app just shows a blank
            // screen.
            val text = "${message.message()} (${message.sourceId()}:${message.lineNumber()})"
            when (message.messageLevel()) {
                ConsoleMessage.MessageLevel.ERROR -> Log.e(TAG_WEB, text)
                ConsoleMessage.MessageLevel.WARNING -> Log.w(TAG_WEB, text)
                else -> Log.i(TAG_WEB, text)
            }
            return true
        }
    }

    private companion object {
        const val TAG = "HomerunHost"
        const val TAG_WEB = "HomerunWeb"

        /** The global the injected backdrop watcher reports through. */
        const val CHROME_INTERFACE = "HomerunChrome"

        /**
         * Separate from `DEBUG_ERROR` in [HomerunApplication] on purpose: two
         * receivers on one action both fire, and that one throws for any mode
         * it does not know.
         */
        const val DEBUG_JS_ERROR_ACTION = "app.gethomerun.mobile.DEBUG_JS_ERROR"

        /** One channel of an `rgb()`/`rgba()`, which is all CSS ever hands back. */
        val CSS_CHANNEL = Regex("""\d+""")

        /**
         * Process-wide, not per-activity: a rotation must not re-prompt, and
         * Android stops showing the dialog after two refusals anyway — asking
         * again would silently do nothing and look like a bug.
         */
        var asked = false
    }
}
