package app.gethomerun.mobile

import android.annotation.SuppressLint
import android.content.Intent
import android.os.Bundle
import android.util.Log
import android.view.ViewGroup
import android.webkit.ConsoleMessage
import android.webkit.RenderProcessGoneDetail
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.widget.FrameLayout
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.lifecycle.lifecycleScope
import androidx.webkit.WebViewAssetLoader
import androidx.webkit.WebViewClientCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json

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
     * Injected before any page script runs. Two jobs:
     *
     *  - `__homerunCapabilities`, which the UI reads **synchronously** at
     *    startup and cannot await.
     *  - `__homerunHost.postMessage`, the name the shared transport looks for.
     *    `addJavascriptInterface` gives us `HomerunHost`; this is the adapter
     *    between that and the protocol's global.
     */
    private val bootstrapScript: String by lazy {
        val capabilities = Json.encodeToString(HostCapabilities.ANDROID)
        """
        (function () {
          window.__homerunCapabilities = $capabilities;
          var host = window.__homerunHost || (window.__homerunHost = {});
          host.postMessage = function (json) { ${BridgeRouter.JS_INTERFACE}.postMessage(json); };
        })();
        """.trimIndent()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        assetLoader = WebBundle.loader(this)
        router = BridgeRouter(applicationContext, lifecycleScope)

        container = FrameLayout(this).apply {
            layoutParams = ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            )
        }
        setContentView(container)

        installWebView()

        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                val view = webView
                if (view != null && view.canGoBack()) view.goBack() else finish()
            }
        })
    }

    /** Builds a fresh WebView, wires it to the router, and loads the bundle. */
    @SuppressLint("SetJavaScriptEnabled")
    private fun installWebView() {
        webView?.let { old ->
            container.removeView(old)
            old.destroy()
        }

        val view = WebView(this)
        view.layoutParams = FrameLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.MATCH_PARENT,
        )

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

        view.loadUrl(WebBundle.START_URL)
    }

    override fun onDestroy() {
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
    }
}
