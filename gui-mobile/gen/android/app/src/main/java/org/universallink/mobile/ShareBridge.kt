package org.universallink.mobile

/**
 * The Kotlin→Rust seam for the share sheet: a shared text goes straight to the
 * embedded Core, without passing through the webview (a share can arrive while
 * the UI is still loading, or while the app is not even started).
 *
 * The Rust side is `Java_org_universallink_mobile_ShareBridge_onShareText` in
 * gui-mobile/src/share.rs — the JNI name is derived from this package, this
 * object and this method name, so renaming any of them breaks the link at
 * runtime (an UnsatisfiedLinkError on the first share), not at build time.
 */
object ShareBridge {
    init {
        // The Tauri runtime's own `Rust` object already loads this library, but
        // only once its class is initialized. Loading it again is a no-op and
        // makes this seam independent of that ordering.
        System.loadLibrary("universallink_gui_mobile")
    }

    /** Hands `text` to the Core, which shares it with the account's devices. */
    @JvmStatic external fun onShareText(text: String)
}
