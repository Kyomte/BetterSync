package com.bettersync

/** The Rust daemon (crates/ffi), loaded from libbsync_ffi.so. */
object Native {
    init {
        System.loadLibrary("bsync_ffi")
    }

    /** Starts the daemon (once); returns the status page URL or "error: …". */
    external fun start(configDir: String): String

    /** Status as JSON (same as GET /api/status) or "error: …". */
    external fun statusJson(configDir: String): String

    external fun stop()
}
