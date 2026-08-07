# The bridge's @JavascriptInterface methods are reached only from JavaScript,
# so nothing in Kotlin references them and R8 would strip them.
-keepclassmembers class * {
    @android.webkit.JavascriptInterface <methods>;
}

# kotlinx.serialization generates serializers reflectively named after the
# class they belong to.
-keepattributes *Annotation*, InnerClasses
-dontnote kotlinx.serialization.**
-keepclassmembers class app.gethomerun.mobile.** {
    *** Companion;
}
-keepclasseswithmembers class app.gethomerun.mobile.** {
    kotlinx.serialization.KSerializer serializer(...);
}
