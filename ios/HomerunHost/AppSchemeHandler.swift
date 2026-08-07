import Foundation
import WebKit

/// Serves the bundled UI over `homerun-app://` instead of `file://`.
///
/// This is not a preference. The bundle is a Next.js export whose entry points
/// are `<script>` tags fetched with CORS semantics; from a `file://` page the
/// origin is opaque, those fetches fail, and nothing is reported — the app
/// shows a blank white screen with an empty console. A custom scheme gives the
/// page a real origin. (Capacitor and Ionic solve it the same way.)
final class AppSchemeHandler: NSObject, WKURLSchemeHandler {
    static let scheme = "homerun-app"
    static let indexURL = URL(string: "\(scheme)://app/index.html")!

    /// UTType guesses badly for exactly the extensions that matter: it maps
    /// `.js` to `application/javascript`, which WebKit refuses to execute as a
    /// module. Spell them out.
    private static let mimeTypes: [String: String] = [
        "html": "text/html",
        "js": "text/javascript",
        "mjs": "text/javascript",
        "css": "text/css",
        "json": "application/json",
        "map": "application/json",
        "txt": "text/plain",
        "svg": "image/svg+xml",
        "png": "image/png",
        "jpg": "image/jpeg",
        "jpeg": "image/jpeg",
        "gif": "image/gif",
        "webp": "image/webp",
        "ico": "image/x-icon",
        "woff": "font/woff",
        "woff2": "font/woff2",
        "ttf": "font/ttf",
        "wasm": "application/wasm",
        "webmanifest": "application/manifest+json",
    ]

    private static let textTypes: Set<String> = [
        "text/html", "text/javascript", "text/css", "application/json", "text/plain",
        "image/svg+xml", "application/manifest+json",
    ]

    /// Tasks WebKit has not stopped yet. See `finish`.
    private var live = Set<ObjectIdentifier>()

    private lazy var root: URL? =
        Bundle.main.resourceURL?.appendingPathComponent("web").standardizedFileURL

    func webView(_ webView: WKWebView, start task: WKURLSchemeTask) {
        live.insert(ObjectIdentifier(task))

        // Without a URL there is nothing to respond against, and a scheme task
        // must be failed rather than left open — the page would wait forever.
        guard let url = task.request.url else {
            task.didFailWithError(URLError(.badURL))
            live.remove(ObjectIdentifier(task))
            return
        }

        guard let root else {
            finish(task, url: url, status: 500, mime: "text/plain", data: Data("No UI bundle".utf8))
            return
        }

        // 404 rather than falling back to index.html for anything that looks
        // like an asset: a missing JS chunk served as HTML fails deep inside
        // the parser, where the real cause is invisible.
        guard let resolved = resolve(path: url.path, in: root),
            let data = try? Data(contentsOf: resolved)
        else {
            finish(task, url: url, status: 404, mime: "text/plain", data: Data("Not found".utf8))
            return
        }

        finish(task, url: url, status: 200, mime: Self.mime(for: resolved), data: data)
    }

    func webView(_ webView: WKWebView, stop task: WKURLSchemeTask) {
        live.remove(ObjectIdentifier(task))
    }

    // MARK: - Path resolution

    /// Maps a request path onto a file in the bundle, or nil for a 404.
    ///
    /// The export is a mix of shapes — `dashboard.html` beside `changelog/` —
    /// so a route can arrive as any of three spellings.
    private func resolve(path rawPath: String, in root: URL) -> URL? {
        var path = rawPath
        if path.isEmpty || path == "/" { path = "/index.html" }

        // The page picks these URLs. Reject traversal before touching the disk,
        // then verify containment after standardizing — the first check catches
        // the obvious attempt, the second catches whatever it missed.
        let components = path.split(separator: "/")
        guard !components.contains("..") else { return nil }

        let base = root.appendingPathComponent(String(path.dropFirst())).standardizedFileURL
        guard base.path == root.path || base.path.hasPrefix(root.path + "/") else { return nil }

        let fm = FileManager.default
        var isDirectory: ObjCBool = false

        if fm.fileExists(atPath: base.path, isDirectory: &isDirectory) {
            if !isDirectory.boolValue { return base }
            let index = base.appendingPathComponent("index.html")
            if fm.fileExists(atPath: index.path) { return index }
        }

        // Extensionless route: `/dashboard` -> `dashboard.html`, and finally
        // the SPA fallback so client-side routes survive a reload.
        if base.pathExtension.isEmpty {
            let asHTML = base.appendingPathExtension("html")
            if fm.fileExists(atPath: asHTML.path) { return asHTML }
            let index = root.appendingPathComponent("index.html")
            if fm.fileExists(atPath: index.path) { return index }
        }

        return nil
    }

    private static func mime(for url: URL) -> String {
        mimeTypes[url.pathExtension.lowercased()] ?? "application/octet-stream"
    }

    private func finish(
        _ task: WKURLSchemeTask, url: URL, status: Int, mime: String, data: Data
    ) {
        // Calling back into a stopped task throws an Objective-C exception that
        // no Swift `catch` will save you from.
        guard live.contains(ObjectIdentifier(task)) else { return }

        var headers = [
            "Content-Type": Self.textTypes.contains(mime) ? "\(mime); charset=utf-8" : mime,
            "Content-Length": String(data.count),
        ]
        // The bundle ships with the app; its contents only change when the app
        // does, and hashed asset names already bust the cache.
        headers["Cache-Control"] = status == 200 ? "public, max-age=31536000" : "no-store"

        guard
            let response = HTTPURLResponse(
                url: url, statusCode: status, httpVersion: "HTTP/1.1", headerFields: headers)
        else { return }

        task.didReceive(response)
        task.didReceive(data)
        task.didFinish()
        live.remove(ObjectIdentifier(task))
    }
}
