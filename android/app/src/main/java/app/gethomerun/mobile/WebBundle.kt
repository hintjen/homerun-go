package app.gethomerun.mobile

import android.content.Context
import android.webkit.WebResourceResponse
import androidx.webkit.WebViewAssetLoader

/**
 * Serves the shared UI bundle out of `assets/web/`.
 *
 * Two things decide this design:
 *
 *  1. **Not `file://`.** The bundle loads `<script>`s that the WebView treats
 *     as cross-origin from an opaque `file://` origin — they fail silently and
 *     you get a blank page with no error. `WebViewAssetLoader` serves the same
 *     files over a real `https://` origin, which also gives us localStorage,
 *     fetch to the backend, and service-worker-shaped behaviour for free.
 *
 *  2. **Next.js static export writes `/dashboard` as `dashboard.html`.**
 *     Client-side navigation never requests those (the router pushes history
 *     without a network fetch), but a reload, a restored session, or a deep
 *     link does. Extensionless paths therefore retry with `.html` appended.
 */
object WebBundle {
    /** Reserved by androidx for exactly this; never resolves in public DNS. */
    const val DOMAIN = "appassets.androidplatform.net"
    const val ORIGIN = "https://$DOMAIN"
    const val START_URL = "$ORIGIN/index.html"

    fun loader(context: Context): WebViewAssetLoader =
        WebViewAssetLoader.Builder()
            .setDomain(DOMAIN)
            .addPathHandler("/", BundlePathHandler(context))
            .build()
}

private class BundlePathHandler(context: Context) : WebViewAssetLoader.PathHandler {
    private val assets = WebViewAssetLoader.AssetsPathHandler(context)

    override fun handle(path: String): WebResourceResponse? {
        val requested = path.trimStart('/')
        val candidate = when {
            requested.isEmpty() -> "index.html"
            requested.endsWith("/") -> requested + "index.html"
            else -> requested
        }

        served("web/$candidate")?.let { return it }

        // Only extensionless paths are routes. Anything with a suffix that
        // missed is a genuinely absent file, and retrying would just mask it.
        if (!candidate.substringAfterLast('/').contains('.')) {
            served("web/$candidate.html")?.let { return it }
        }

        return served("web/404.html")
    }

    /**
     * `AssetsPathHandler` reports a miss as either null or — depending on the
     * androidx version — a response whose stream is null. Treat both as a miss
     * so the fallbacks above actually run.
     */
    private fun served(assetPath: String): WebResourceResponse? =
        assets.handle(assetPath)?.takeIf { it.data != null }
}
