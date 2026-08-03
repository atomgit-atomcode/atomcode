package com.atomcode.jetbrains.daemon

import com.atomcode.jetbrains.settings.AtomCodeSettings
import java.io.File
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption
import java.security.MessageDigest
import java.util.HexFormat
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

private const val MAX_DAEMON_STDERR_CHARS = 8_192

internal data class DaemonProcessExit(
    val exitCode: Int,
    val stderr: String,
)

internal interface ManagedDaemonProcess {
    fun isAlive(): Boolean
    fun onExit(): CompletableFuture<DaemonProcessExit>
    fun destroy()
}

internal sealed interface DaemonLaunchResult {
    data object MissingBinary : DaemonLaunchResult
    data class Started(val process: ManagedDaemonProcess) : DaemonLaunchResult
    data class Failed(val message: String) : DaemonLaunchResult
}

internal interface DaemonProcessLauncher {
    fun expectedVersion(): String?
    fun expectedHash(): String?
    fun start(): CompletableFuture<DaemonLaunchResult>
}

internal class AtomCodeDaemonProcess(
    private val settings: AtomCodeSettings,
) : DaemonProcessLauncher {
    private companion object {
        val EXTRACTION_LOCK = Any()
    }

    fun locateBinary(): BinaryResolution? {
        configuredBinary()?.let { return it }
        bundledDaemon()?.let { return BinaryResolution(it.toString(), emptyList()) }
        // On Windows the standalone `atomcode-daemon` binary is a GUI-subsystem
        // app (no console window when spawned from the IDE), while the
        // `atomcode` CLI is a console-subsystem app that flashes a cmd window.
        // Prefer the daemon binary over the CLI on Windows; keep the CLI-first
        // order elsewhere since both behave identically there.
        if (isWindows()) {
            commonDaemonPaths().firstOrNull { Files.isRegularFile(it) }?.let {
                return BinaryResolution(it.toString(), emptyList())
            }
            developerDaemonPaths().firstOrNull { Files.isRegularFile(it) }?.let {
                return BinaryResolution(it.toString(), emptyList())
            }
        }
        pathBinary("atomcode")?.let { return BinaryResolution(it.toString(), listOf("daemon")) }
        commonAtomcodePaths().firstOrNull { Files.isRegularFile(it) }?.let {
            return BinaryResolution(it.toString(), listOf("daemon"))
        }
        // On Windows the daemon paths were already probed above and would never
        // newly succeed here — keep the tail check for macOS/Linux only.
        if (!isWindows()) {
            commonDaemonPaths().firstOrNull { Files.isRegularFile(it) }?.let {
                return BinaryResolution(it.toString(), emptyList())
            }
            developerDaemonPaths().firstOrNull { Files.isRegularFile(it) }?.let {
                return BinaryResolution(it.toString(), emptyList())
            }
        }
        return null
    }

    fun expectedBundledVersion(): String? {
        if (settings.daemonBinaryPath.trim().isNotEmpty()) return null
        if (!hasBundledDaemonResource()) return null
        val loader = AtomCodeDaemonProcess::class.java.classLoader
        return loader.getResourceAsStream("resources/bin/daemon-version.txt")?.use { stream ->
            stream.bufferedReader().readText().trim().takeIf { it.isNotBlank() }
        }
    }

    fun expectedBundledHash(): String? {
        if (settings.daemonBinaryPath.trim().isNotEmpty()) return null
        val platformDir = platformDir() ?: return null
        val executable = executableName("atomcode-daemon")
        val resourcePath = "resources/bin/$platformDir/$executable"
        val loader = AtomCodeDaemonProcess::class.java.classLoader
        return loader.getResourceAsStream(resourcePath)?.use { stream ->
            val digest = MessageDigest.getInstance("SHA-256")
            val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
            while (true) {
                val count = stream.read(buffer)
                if (count < 0) break
                digest.update(buffer, 0, count)
            }
            HexFormat.of().formatHex(digest.digest())
        }
    }

    override fun expectedVersion(): String? = expectedBundledVersion()

    override fun expectedHash(): String? = expectedBundledHash()

    override fun start(): CompletableFuture<DaemonLaunchResult> =
        CompletableFuture.supplyAsync {
            try {
                val binary = locateBinary() ?: return@supplyAsync DaemonLaunchResult.MissingBinary
                val args = mutableListOf<String>()
                args += binary.path
                args += binary.argsPrefix
                args += listOf("--port", settings.port.toString(), "--client", "jetbrains")
                // Windows only: disable the daemon's idle shutdown (default 30 min)
                // so an idled daemon doesn't exit and get restarted by the next sent
                // message — which flashes a console window when the fallback CLI
                // binary is used. On macOS/Linux the restart is windowless, and the
                // idle self-shutdown is the only cleanup for a daemon orphaned by a
                // hard IDE termination (crash / kill -9, where dispose() never runs),
                // so keep the watchdog there.
                if (isWindows()) {
                    args += listOf("--idle-timeout", "0")
                }

                val builder = ProcessBuilder(args)
                    .redirectOutput(ProcessBuilder.Redirect.DISCARD)
                    .redirectError(ProcessBuilder.Redirect.PIPE)
                normalizeDaemonEnvForUtf8Locale(builder.environment())
                val process = builder.start()
                DaemonLaunchResult.Started(JvmManagedDaemonProcess(process))
            } catch (error: Exception) {
                DaemonLaunchResult.Failed(error.message ?: error.javaClass.simpleName)
            }
        }

    private fun configuredBinary(): BinaryResolution? {
        val raw = settings.daemonBinaryPath.trim()
        if (raw.isEmpty()) return null
        val path = expandHome(raw)
        if (!Files.isRegularFile(path)) return null
        val name = path.fileName.toString()
        return if (name.contains("daemon")) {
            BinaryResolution(path.toString(), emptyList())
        } else {
            BinaryResolution(path.toString(), listOf("daemon"))
        }
    }

    private fun bundledDaemon(): Path? {
        val platformDir = platformDir() ?: return null
        val executable = executableName("atomcode-daemon")
        val resourcePath = "resources/bin/$platformDir/$executable"
        val contentHash = expectedBundledHash() ?: return null
        val loader = AtomCodeDaemonProcess::class.java.classLoader
        val destination = Path.of(
            System.getProperty("java.io.tmpdir"),
            "atomcode-jetbrains",
            "bin",
            platformDir,
            contentHash.take(16),
            ProcessHandle.current().pid().toString(),
            executable,
        )
        synchronized(EXTRACTION_LOCK) {
            if (Files.isRegularFile(destination)) return destination
            loader.getResourceAsStream(resourcePath)?.use { stream ->
                Files.createDirectories(destination.parent)
                Files.copy(stream, destination, StandardCopyOption.REPLACE_EXISTING)
                if (!isWindows()) {
                    destination.toFile().setExecutable(true, false)
                }
                return destination
            }
        }
        return null
    }

    private fun hasBundledDaemonResource(): Boolean {
        val platformDir = platformDir() ?: return false
        val executable = executableName("atomcode-daemon")
        val resourcePath = "resources/bin/$platformDir/$executable"
        return AtomCodeDaemonProcess::class.java.classLoader.getResource(resourcePath) != null
    }

    private fun pathBinary(name: String): Path? {
        val candidates = System.getenv("PATH")
            .orEmpty()
            .split(File.pathSeparator)
            .filter { it.isNotBlank() }
            .map { Path.of(it, executableName(name)) }
        return candidates.firstOrNull { Files.isRegularFile(it) }
    }

    private fun commonAtomcodePaths(): List<Path> = listOf(
        "~/.atomcode/bin/atomcode",
        "~/.cargo/bin/atomcode",
        "/usr/local/bin/atomcode",
    ).map(::expandHome)

    private fun commonDaemonPaths(): List<Path> = listOf(
        "~/.atomcode/bin/atomcode-daemon",
        "~/.cargo/bin/atomcode-daemon",
        "/usr/local/bin/atomcode-daemon",
    ).map { executableName(it) }.map(::expandHome)

    private fun developerDaemonPaths(): List<Path> = listOf(
        "target/release/atomcode-daemon",
        "target/debug/atomcode-daemon",
    ).map { executableName(it) }.map { Path.of(it).toAbsolutePath() }

    private fun executableName(name: String): String =
        if (isWindows() && !name.endsWith(".exe")) "$name.exe" else name

    private fun isWindows(): Boolean =
        System.getProperty("os.name").lowercase().contains("win")

    private fun platformDir(): String? {
        val os = System.getProperty("os.name").lowercase()
        val arch = System.getProperty("os.arch").lowercase()
        val normalizedArch = when (arch) {
            "aarch64", "arm64" -> "arm64"
            "x86_64", "amd64" -> "x64"
            else -> arch
        }
        return when {
            os.contains("mac") && normalizedArch == "arm64" -> "darwin-arm64"
            os.contains("mac") && normalizedArch == "x64" -> "darwin-x64"
            os.contains("linux") && normalizedArch == "arm64" -> "linux-arm64"
            os.contains("linux") && normalizedArch == "x64" -> "linux-x64"
            os.contains("win") && normalizedArch == "x64" -> "win32-x64"
            else -> null
        }
    }

    private fun expandHome(path: String): Path {
        val expanded = if (path == "~" || path.startsWith("~/")) {
            System.getProperty("user.home") + path.removePrefix("~")
        } else {
            path
        }
        return Path.of(expanded)
    }
}

internal class JvmManagedDaemonProcess(
    private val process: Process,
) : ManagedDaemonProcess {
    private val stderr = CompletableFuture.supplyAsync {
        process.errorStream.bufferedReader().use { reader ->
            val tail = StringBuilder()
            val buffer = CharArray(1_024)
            while (true) {
                val count = reader.read(buffer)
                if (count < 0) break
                val overflow = tail.length + count - MAX_DAEMON_STDERR_CHARS
                if (overflow > 0) tail.delete(0, overflow.coerceAtMost(tail.length))
                tail.append(buffer, 0, count)
            }
            tail.toString()
        }
    }.handle { diagnostic, _ -> diagnostic.orEmpty() }
    private val exit = process.onExit().thenCombine(stderr) { exited, diagnostic ->
        DaemonProcessExit(exited.exitValue(), diagnostic.trim())
    }

    override fun isAlive(): Boolean = process.isAlive

    override fun onExit(): CompletableFuture<DaemonProcessExit> = exit

    override fun destroy() {
        if (!process.isAlive) return
        process.destroy()
        if (!process.waitFor(2, TimeUnit.SECONDS)) process.destroyForcibly()
    }
}

fun normalizeDaemonEnvForUtf8Locale(
    env: MutableMap<String, String>,
    osName: String = System.getProperty("os.name"),
) {
    val normalizedOs = osName.lowercase()
    if (normalizedOs.contains("win")) return
    if (isUtf8Locale(env["LC_ALL"]) && !isBareUtf8Locale(env["LC_ALL"])) return
    if (isCLocale(env["LC_ALL"]) || isBareUtf8Locale(env["LC_ALL"])) {
        env.remove("LC_ALL")
    }

    val isMac = normalizedOs.contains("mac")
    val ctypeFallback = if (isMac) {
        "UTF-8"
    } else {
        "C.UTF-8"
    }
    val langFallback = if (isMac) {
        "en_US.UTF-8"
    } else {
        "C.UTF-8"
    }
    val hasUtf8Ctype = isUtf8Locale(env["LC_CTYPE"]) &&
        (isMac || !isBareUtf8Locale(env["LC_CTYPE"]))
    if (!hasUtf8Ctype) {
        env["LC_CTYPE"] = ctypeFallback
    }
    if (isCLocale(env["LANG"]) || isBareUtf8Locale(env["LANG"])) {
        env["LANG"] = langFallback
    }
}

private fun isCLocale(value: String?): Boolean {
    val normalized = value?.trim()?.lowercase().orEmpty()
    return normalized.isEmpty() || normalized == "c" || normalized == "posix"
}

private fun isUtf8Locale(value: String?): Boolean {
    val normalized = value?.trim()?.lowercase().orEmpty()
    return normalized.contains("utf-8") || normalized.contains("utf8")
}

private fun isBareUtf8Locale(value: String?): Boolean {
    val normalized = value?.trim()?.lowercase().orEmpty()
    return normalized == "utf-8" || normalized == "utf8"
}
