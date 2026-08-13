package app.gethomerun.mobile

import android.Manifest
import android.annotation.SuppressLint
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.ViewGroup
import android.webkit.ConsoleMessage
import android.webkit.JavascriptInterface
import android.webkit.RenderProcessGoneDetail
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.widget.FrameLayout
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
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

    /**
     * The space the system bars occupy; zero until the first inset pass.
     *
     * Written on the UI thread and read from a binder thread by
     * `ChromeInterface.safeArea`, which is why it is volatile.
     */
    @Volatile
    private var bars: Insets = Insets.NONE

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
        }

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
            if (state != ServerState.STARTING) return
            runOnUiThread { askForNotifications() }
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
     * dispatcher falls through to the platform and both come back. It is the
     * default from targetSdk 36, which Play requires by 31 Aug 2026.
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
         */
        @JavascriptInterface
        fun safeArea(): String {
            val density = resources.displayMetrics.density
            val bars = bars
            // Locale.US, not the default: a device set to a comma-decimal
            // locale would format 21.33 as "21,33px", which is not a CSS
            // length. The page would silently keep its zeroes and draw under
            // the bars in exactly the places this exists to prevent.
            return "%.2fpx %.2fpx %.2fpx %.2fpx".format(
                Locale.US,
                bars.top / density,
                bars.right / density,
                bars.bottom / density,
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
     * The IME is deliberately not in this: `adjustResize` in the manifest
     * already shrinks the window when the keyboard opens. Adding `Type.ime()`
     * would take the keyboard's height out a second time and leave a gap the
     * size of the keyboard above it.
     */
    private fun holdBarsOutOfThePage() {
        ViewCompat.setOnApplyWindowInsetsListener(container) { _, insets ->
            bars = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout(),
            )
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

        container = FrameLayout(this).apply {
            layoutParams = ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            )
        }
        setContentView(container)
        holdBarsOutOfThePage()

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

        // Offer it as soon as it is ready, rather than waiting for the user to
        // relaunch of their own accord. `update-available` is what the shared
        // UI's update prompt subscribes to.
        BundleUpdater.onBundleStaged = { bundle ->
            router.emit(
                "update-available",
                listOf(
                    buildJsonObject {
                        put("status", "available")
                        put("version", bundle)
                        put("bundle", bundle)
                    }
                ),
            )
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

        ServerHost.addListener(hostingListener)

        onBackPressedDispatcher.addCallback(this, backToPreviousPage)
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
    }

    /**
     * Back from the background, where a server may have been running the whole
     * time. The page is told what is true now rather than left with whatever it
     * last heard (PROTOCOL.md §4.3).
     */
    override fun onResume() {
        super.onResume()
        router.resyncServerState()
        // The other half of "launch and resume". A phone that is never closed
        // would otherwise check once and stay on that bundle for weeks; the
        // throttle inside means most resumes cost nothing.
        BundleUpdater.check(this, lifecycleScope)
    }

    private fun askForNotifications() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (asked) return
        asked = true
        val granted = ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED
        if (granted) return
        runCatching { requestNotifications.launch(Manifest.permission.POST_NOTIFICATIONS) }
            .onFailure { Log.w(TAG, "could not ask about notifications: ${it.message}") }
    }

    override fun onDestroy() {
        ServerHost.removeListener(hostingListener)
        // The router is built in `onCreate` and subscribes to `ServerHost`,
        // which outlives every activity — so it has to be let go here for the
        // same reason the listener above does. Missing this left one abandoned
        // router per recreation, each still reporting server state to the API.
        // Guarded because `onCreate` can throw before the assignment, and an
        // UninitializedPropertyAccessException here would bury whatever did it.
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
         * The render process was killed — most likely by memory pressure,
         * which is exactly what hosting a Minecraft server on a phone causes.
         * The WebView is unusable from here; returning true means "handled,
         * do not kill the app".
         */
        override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
            Log.w(TAG, "render process gone (crashed=${detail.didCrash()}); rebuilding")
            router.onPageGone()
            if (!isFinishing && !isDestroyed) installWebView()
            return true
        }
    }

    private inner class ConsoleClient : WebChromeClient() {
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
