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
