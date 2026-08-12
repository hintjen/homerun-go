package app.gethomerun.mobile

import android.content.Context
import android.webkit.WebResourceResponse
import androidx.webkit.WebViewAssetLoader
import java.io.File

/**
 * Serves the shared UI bundle — either the copy inside the APK, or one
 * delivered over the air.
 *
 * Three things decide this design:
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
 *
 *  3. **One root per page, chosen before the page loads.** Which bundle is live
 *     is [BundleStore]'s decision; this only serves it. Falling back to the
 *     shipped copy *per request* would be the obvious-looking alternative and
 *     is wrong — it would mix two builds, serving one bundle's HTML against
 *     another's chunks. A bundle is usable or it is not, and that is judged
 *     once, at [BundleStore.resolve].
 */
object WebBundle {
    /** Reserved by androidx for exactly this; never resolves in public DNS. */
    const val DOMAIN = "appassets.androidplatform.net"
    const val ORIGIN = "https://$DOMAIN"
    const val START_URL = "$ORIGIN/index.html"

    /**
     * @param root a downloaded bundle directory, or null for `assets/web/`.
     */
    fun loader(context: Context, root: File?): WebViewAssetLoader =
        WebViewAssetLoader.Builder()
            .setDomain(DOMAIN)
            .addPathHandler("/", BundlePathHandler(source(context, root)))
            .build()

    /**
     * A downloaded bundle whose directory turns out to be unopenable falls back
     * to the shipped copy rather than throwing.
     *
     * `InternalStoragePathHandler`'s constructor rejects a directory outside
     * the app's own storage, which cannot happen from [BundleStore] — but this
     * runs in `onCreate`, and the difference between a blank screen and an
     * app that will not start at all is worth a `runCatching`.
     */
    private fun source(context: Context, root: File?): Source {
        if (root == null) return AssetSource(context)
        return runCatching { DirectorySource(context, root) }.getOrElse { AssetSource(context) }
    }
}

/** Where one page's files come from. */
private interface Source {
    fun open(relative: String): WebResourceResponse?
}

/** The copy compiled into the APK, under `assets/web/`. */
private class AssetSource(context: Context) : Source {
    private val assets = WebViewAssetLoader.AssetsPathHandler(context)
    override fun open(relative: String): WebResourceResponse? = assets.handle("web/$relative")
}

/**
 * A bundle on disk.
 *
 * `InternalStoragePathHandler` rather than reading the file ourselves: it does
 * the canonical-path check that stops `../../shared_prefs/...` escaping the
 * bundle directory, and guesses content types. Both are things to get from
 * androidx rather than to write again here.
 */
private class DirectorySource(context: Context, root: File) : Source {
    private val storage = WebViewAssetLoader.InternalStoragePathHandler(context, root)
    override fun open(relative: String): WebResourceResponse? = storage.handle(relative)
}

private class BundlePathHandler(private val source: Source) : WebViewAssetLoader.PathHandler {

    override fun handle(path: String): WebResourceResponse? {
        val requested = path.trimStart('/')
        val candidate = when {
            requested.isEmpty() -> "index.html"
            requested.endsWith("/") -> requested + "index.html"
            else -> requested
        }

        served(candidate)?.let { return it }

        // Only extensionless paths are routes. Anything with a suffix that
        // missed is a genuinely absent file, and retrying would just mask it.
        if (!candidate.substringAfterLast('/').contains('.')) {
            served("$candidate.html")?.let { return it }
        }

        return served("404.html")
    }

    /**
     * Both androidx handlers report a miss as either null or — depending on the
     * version — a response whose stream is null. Treat both as a miss so the
     * fallbacks above actually run.
     */
    private fun served(relative: String): WebResourceResponse? =
        source.open(relative)?.takeIf { it.data != null }
}
