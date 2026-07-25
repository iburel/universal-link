package org.universallink.mobile

/**
 * The Kotlin→Rust seam for the share sheet: a shared text goes straight to the
 * embedded Core, without passing through the webview (a share can arrive while
 * the UI is still loading, or while the app is not even started).
 *
 * The Rust side is `Java_org_universallink_mobile_ShareBridge_onShareText` (and
 * `…_onShareFiles`) in gui-mobile/src/share.rs — the JNI name is derived from
 * this package, this object and the method name, so renaming any of them breaks
 * the link at runtime (an UnsatisfiedLinkError on the first share), not at build
 * time.
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

    /**
     * Reports a file share, as JSON (`file_share_status` in share.rs):
     *
     *     {"phase":"preparing","files":2}
     *     {"phase":"pick","dir":"…/shares/<id>","files":[{"path":…,"name":…,"size":…}]}
     *     {"phase":"failed","reason":"unreadable"|"no_space"}
     *
     * Built by [ShareFiles]. Unlike a text share this never reaches the Core from
     * the Rust side: a file needs a destination, and the UI is what asks for one.
     */
    @JvmStatic external fun onShareFiles(json: String)
}
