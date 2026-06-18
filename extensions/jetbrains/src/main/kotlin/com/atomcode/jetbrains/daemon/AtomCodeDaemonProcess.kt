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

class AtomCodeDaemonProcess(
    private val settings: AtomCodeSettings,
) {
    private val processLock = Any()

    @Volatile
    private var ownedProcess: Process? = null

    fun locateBinary(): BinaryResolution? {
        configuredBinary()?.let { return it }
        bundledDaemon()?.let { return BinaryResolution(it.toString(), emptyList()) }
        pathBinary("atomcode")?.let { return BinaryResolution(it.toString(), listOf("daemon")) }
        commonAtomcodePaths().firstOrNull { Files.isRegularFile(it) }?.let {
            return BinaryResolution(it.toString(), listOf("daemon"))
        }
        commonDaemonPaths().firstOrNull { Files.isRegularFile(it) }?.let {
            return BinaryResolution(it.toString(), emptyList())
        }
        developerDaemonPaths().firstOrNull { Files.isRegularFile(it) }?.let {
            return BinaryResolution(it.toString(), emptyList())
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

    fun ensureRunning(auth: DaemonAuth): CompletableFuture<Boolean> =
        CompletableFuture.supplyAsync {
            val binary = locateBinary() ?: return@supplyAsync false
            val args = mutableListOf<String>()
            args += binary.path
            args += binary.argsPrefix
            args += listOf("--port", settings.port.toString(), "--client", "jetbrains")

            val process = ProcessBuilder(args)
                .redirectOutput(ProcessBuilder.Redirect.DISCARD)
                .redirectError(ProcessBuilder.Redirect.DISCARD)
                .start()
            synchronized(processLock) {
                ownedProcess?.takeIf { it.isAlive }?.let {
                    it.destroy()
                    it.waitFor(2, TimeUnit.SECONDS)
                }
                ownedProcess = process
            }
            true
        }

    fun restartOwnedDaemon(auth: DaemonAuth): CompletableFuture<Boolean> =
        CompletableFuture.supplyAsync {
            synchronized(processLock) {
                ownedProcess?.let {
                    if (it.isAlive) {
                        it.destroy()
                        if (!it.waitFor(2, TimeUnit.SECONDS)) {
                            it.destroyForcibly()
                        }
                    }
                }
                ownedProcess = null
            }
            ensureRunning(auth).get(10, TimeUnit.SECONDS)
        }

    fun isOwnedProcess(): Boolean = synchronized(processLock) {
        ownedProcess?.isAlive == true
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
        val loader = AtomCodeDaemonProcess::class.java.classLoader
        loader.getResourceAsStream(resourcePath)?.use { stream ->
            val destination = Path.of(
                System.getProperty("java.io.tmpdir"),
                "atomcode-jetbrains",
                "bin",
                platformDir,
                executable,
            )
            Files.createDirectories(destination.parent)
            Files.copy(stream, destination, StandardCopyOption.REPLACE_EXISTING)
            if (!System.getProperty("os.name").lowercase().contains("win")) {
                destination.toFile().setExecutable(true, false)
            }
            return destination
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
        if (System.getProperty("os.name").lowercase().contains("win") && !name.endsWith(".exe")) "$name.exe" else name

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
