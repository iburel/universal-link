package dev.universallink.app

/** Bridge to the embedded Rust Core (libuniversallink_android.so). */
object NativeCore {
    init {
        System.loadLibrary("universallink_android")
    }

    /**
     * Boots the embedded Core with [dataDir] as its app-private data dir and
     * runs the on-device smoke checks (iroh bind + TLS egress). Returns a
     * human-readable summary. Blocks for up to ~20 s — call off the main
     * thread.
     */
    external fun nativeStart(dataDir: String): String
}
