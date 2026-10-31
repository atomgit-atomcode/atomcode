package com.atomcode.app

/**
 * The JNI surface of `crates/atomcode-android`.
 *
 * Nothing here decides anything: the Rust side owns the runtime, the session log
 * and the HTTP server. This object only forwards strings.
 *
 * The library is loaded once per process; `nativeStart` is idempotent, so an
 * `Activity` recreation or a `WebView` reload returns the URL of the server that
 * is already running instead of starting a second one.
 */
object NativeAgent {
    init {
        System.loadLibrary("atomcode_android")
    }

    /** Install the logcat logger. 2=warn 3=info 4=debug 5=trace. */
    external fun nativeInit(level: Int)

    /**
     * Start (or return) the in-process agent server.
     *
     * @param dataDir the app-private tree that replaces `~/.atomcode`
     * @param workspace the directory the agent starts working in
     * @param nativeLibDir `applicationInfo.nativeLibraryDir`, where the bundled
     *   `git` (`libgit.so`) is installed — the only directory Android 10+ lets
     *   an app execute binaries from. The Rust host writes `sh` wrappers for it
     *   and puts them on `PATH`.
     * @param helperLibDir the optional helper APK's `nativeLibraryDir`, or null
     *   when it is not installed. Its tools (`make`, `tcc`) spawn child
     *   processes, which the plugin system cannot host; Android does allow
     *   executing another package's library directory, so the helper only holds
     *   files. The host links its tools into `<data>/bin`.
     * @return the webui URL to load, or null on failure
     */
    external fun nativeStart(
        dataDir: String,
        workspace: String,
        nativeLibDir: String,
        helperLibDir: String?,
    ): String?

    /** Stop the server and release the runtime. Idempotent. */
    external fun nativeStop()

    /**
     * Whether this build can sign requests for the AtomGit gateway.
     *
     * `false` means the hosted gateway (`llm-api.atomgit.com`) rejects every
     * request with `ATOMCODE_SIG_MISSING`, so a turn against it cannot succeed.
     */
    external fun nativeGatewaySigningAvailable(): Boolean

    /** Why the last [nativeStart] failed, or null. */
    external fun nativeLastError(): String?
}