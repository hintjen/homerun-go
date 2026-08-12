package app.gethomerun.mobile

import android.app.Application
import android.os.Build
import android.webkit.WebView

class HomerunApplication : Application() {
    override fun onCreate() {
        super.onCreate()

        // Process-scoped, because a running server must survive the activity
        // and the WebView being torn down and rebuilt.
        ServerHost.init(this)

        // The heartbeat starts with the process, not with a login or a server.
        // The API marks a device unhealthy 60 s after its last report, and a
        // relaunch already holds the token issued last time.
        DeviceRegistry.init(this) { ServerHost.backend.runningServerIds }

        // Process-scoped for the same reason the backend is: the device link
        // takes up to a minute to provision and must outlive any page. The
        // scope is ServerHost's — cancelled never, because the thing that ends
        // it is the process ending.
        DeviceWebsocket.init(this, ServerHost.scope)

        // Debug builds are inspectable from the host machine at
        // chrome://inspect — the only practical way to debug the shared UI
        // running inside the app.
        if (BuildConfig.DEBUG) {
            WebView.setWebContentsDebuggingEnabled(true)
        }

        // Every process that touches a WebView needs its own data directory.
        // Hosting will eventually run in its own process, and the second
        // WebView to start would otherwise abort the app on a lock conflict
        // whose stack trace says nothing about the real cause.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            val process = Application.getProcessName()
            if (process != packageName) {
                WebView.setDataDirectorySuffix(process.substringAfter(':', "alt"))
            }
        }
    }
}
