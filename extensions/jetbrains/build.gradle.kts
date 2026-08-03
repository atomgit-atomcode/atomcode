import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption
import java.util.concurrent.TimeUnit

plugins {
    kotlin("jvm") version "2.2.21"
    kotlin("plugin.serialization") version "2.2.21"
    id("org.jetbrains.intellij.platform") version "2.16.0"
}

group = "com.atomcode"
version = providers.gradleProperty("pluginVersion").get()

val platformLocalPath = providers.gradleProperty("platformLocalPath")
val platformVersion = providers.gradleProperty("platformVersion")
val targetPlatformBaseline = providers.provider {
    val localBaseline = platformLocalPath.orNull?.let(::localIdeBaselineVersion)
    val configuredBaseline = platformBaselineVersion(platformVersion.get())
    localBaseline ?: configuredBaseline ?: 0
}
val targetUsesUnifiedIdea = targetPlatformBaseline.map { it >= 253 }
val targetUsesSeparateJcefPlugin = targetPlatformBaseline.map { it >= 262 }

repositories {
    mavenCentral()
    intellijPlatform {
        defaultRepositories()
    }
}

java {
    toolchain {
        languageVersion.set(JavaLanguageVersion.of(21))
    }
}

kotlin {
    jvmToolchain(21)
    compilerOptions {
        freeCompilerArgs.add("-Xjvm-default=all")
    }
}

dependencies {
    implementation("com.google.code.gson:gson:2.11.0")  // 保留，逐步迁移后移除
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.8.1")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-swing:1.10.2")
    implementation("org.jetbrains.kotlinx:kotlinx-collections-immutable:0.3.8")

    intellijPlatform {
        if (platformLocalPath.isPresent) {
            local(platformLocalPath)
        } else if (targetUsesUnifiedIdea.get()) {
            intellijIdea(platformVersion.get())
        } else {
            intellijIdeaCommunity(platformVersion.get())
        }
        if (targetUsesSeparateJcefPlugin.get()) {
            bundledPlugin("intellij.platform.ui.jcef")
        }

        pluginVerifier()
        zipSigner()
    }

    testImplementation(kotlin("test-junit5"))
    testRuntimeOnly("junit:junit:4.13.2")
}

intellijPlatform {
    pluginConfiguration {
        id = "com.atomcode.jetbrains"
        name = "AtomCode"
        version = providers.gradleProperty("pluginVersion")

        ideaVersion {
            sinceBuild = providers.gradleProperty("pluginSinceBuild")
            untilBuild = provider { null }
        }
    }

    pluginVerification {
        ides {
            if (platformLocalPath.isPresent) {
                local(platformLocalPath)
            } else {
                recommended()
            }
        }
    }

    signing {
        certificateChainFile = layout.file(
            providers.environmentVariable("JETBRAINS_CERTIFICATE_CHAIN_FILE").map { file(it) },
        )
        privateKeyFile = layout.file(
            providers.environmentVariable("JETBRAINS_PRIVATE_KEY_FILE").map { file(it) },
        )
        password = providers.environmentVariable("JETBRAINS_PRIVATE_KEY_PASSWORD")
    }

    publishing {
        token = providers.environmentVariable("JETBRAINS_PUBLISH_TOKEN")
        channels = listOf(providers.environmentVariable("JETBRAINS_CHANNEL").orElse("beta").get())
    }
}

tasks {
    named("verifyPluginSignature") {
        dependsOn("signPlugin")
    }

    val skipSearchableOptions = providers.gradleProperty("skipSearchableOptions")
        .map(String::toBoolean)
        .orElse(false)
    val repoRoot = layout.projectDirectory.dir("../..").asFile.toPath().normalize()
    val bundledDaemonDir = layout.buildDirectory.dir("generated/bundledDaemon")
    val daemonTargets = listOf(
        DaemonTarget("darwin-arm64", "atomcode-daemon", "ATOMCODE_DAEMON_DARWIN_ARM64", "aarch64-apple-darwin"),
        DaemonTarget("darwin-x64", "atomcode-daemon", "ATOMCODE_DAEMON_DARWIN_X64", "x86_64-apple-darwin"),
        DaemonTarget("linux-x64", "atomcode-daemon", "ATOMCODE_DAEMON_LINUX_X64", "x86_64-unknown-linux-gnu"),
        DaemonTarget("linux-arm64", "atomcode-daemon", "ATOMCODE_DAEMON_LINUX_ARM64", "aarch64-unknown-linux-gnu"),
        DaemonTarget("win32-x64", "atomcode-daemon.exe", "ATOMCODE_DAEMON_WIN32_X64", "x86_64-pc-windows-msvc"),
    )
    val currentTargetId = currentDaemonTargetId()
    val currentDaemonTarget = daemonTargets.firstOrNull { it.id == currentTargetId }

    // The AtomGit gateway signer is supplied only by build-official.sh. Never run a
    // plain Cargo build here: it would overwrite the official daemon with the stub
    // implementation at the exact same target/release path.
    val verifyOfficialDaemonForRunIde by registering {
        val executable = currentDaemonTarget?.executable ?: "atomcode-daemon"
        val daemon = repoRoot.resolve("target/release/$executable")
        onlyIf {
            currentDaemonTarget != null &&
                providers.environmentVariable(currentDaemonTarget.env).orNull.isNullOrBlank()
        }
        doLast {
            if (!Files.isRegularFile(daemon)) {
                throw GradleException(
                    "Official AtomCode daemon is missing. Run ./build-official.sh from the repository root before runIde."
                )
            }
            val process = ProcessBuilder(daemon.toAbsolutePath().toString(), "--check-official-build")
                .redirectOutput(ProcessBuilder.Redirect.DISCARD)
                .redirectError(ProcessBuilder.Redirect.DISCARD)
                .start()
            if (!process.waitFor(5, TimeUnit.SECONDS)) {
                process.destroyForcibly()
                throw GradleException(
                    "target/release/$executable does not support official-build verification. " +
                        "Run ./build-official.sh again before runIde."
                )
            }
            if (process.exitValue() != 0) {
                throw GradleException(
                    "target/release/$executable does not contain the official AtomGit signer. " +
                        "Run ./build-official.sh again before runIde."
                )
            }
        }
    }

    val bundleDaemon by registering {
        val daemonSources = providers.provider {
            daemonTargets.mapNotNull { target ->
                providers.environmentVariable(target.env).orNull
                    ?.let { repoRoot.fileSystem.getPath(it).toAbsolutePath().normalize() }
                    ?: localDaemonCandidate(repoRoot, target, currentTargetId)
            }.map { it.toFile() }
        }
        inputs.files(daemonSources)
        outputs.dir(bundledDaemonDir)
        mustRunAfter(verifyOfficialDaemonForRunIde)
        doLast {
            val outputRoot = bundledDaemonDir.get().asFile.toPath()
            Files.createDirectories(outputRoot)
            var copied = 0
            daemonTargets.forEach { target ->
                val explicit = providers.environmentVariable(target.env).orNull
                    ?.let { repoRoot.fileSystem.getPath(it).toAbsolutePath().normalize() }
                val source = explicit ?: localDaemonCandidate(repoRoot, target, currentTargetId)
                if (source == null || !Files.isRegularFile(source)) {
                    return@forEach
                }
                val destination = outputRoot.resolve("resources/bin/${target.id}/${target.executable}")
                Files.createDirectories(destination.parent)
                Files.copy(source, destination, StandardCopyOption.REPLACE_EXISTING)
                if (!target.executable.endsWith(".exe")) {
                    destination.toFile().setExecutable(true, false)
                }
                copied += 1
                logger.lifecycle("[bundleDaemon] ${target.id}: $source -> $destination")
            }

            val version = parseWorkspaceVersion(repoRoot)
            if (version != null) {
                val versionFile = outputRoot.resolve("resources/bin/daemon-version.txt")
                Files.createDirectories(versionFile.parent)
                Files.writeString(versionFile, version)
            }

            if (copied == 0) {
                logger.warn("[bundleDaemon] no daemon binary bundled; plugin will use configured/PATH/common daemon locations.")
            }
        }
    }

    test {
        useJUnitPlatform()
    }

    processResources {
        dependsOn(bundleDaemon)
        from(bundledDaemonDir)
    }

    named("runIde") {
        dependsOn(verifyOfficialDaemonForRunIde)
    }

    listOf("buildSearchableOptions", "prepareJarSearchableOptions", "jarSearchableOptions").forEach { taskName ->
        named(taskName) {
            enabled = !skipSearchableOptions.get()
        }
    }
}

data class DaemonTarget(
    val id: String,
    val executable: String,
    val env: String,
    val rustTriple: String,
)

fun currentDaemonTargetId(): String? {
    val os = System.getProperty("os.name").lowercase()
    val arch = System.getProperty("os.arch").lowercase()
    val normalizedArch = when {
        arch == "aarch64" || arch == "arm64" -> "arm64"
        arch == "x86_64" || arch == "amd64" -> "x64"
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

fun localDaemonCandidate(repoRoot: java.nio.file.Path, target: DaemonTarget, currentTargetId: String?): java.nio.file.Path? {
    if (target.id != currentTargetId) return null
    return listOf(
        repoRoot.resolve("target/release/${target.executable}"),
        repoRoot.resolve("target/debug/${target.executable}"),
        repoRoot.resolve("target/${target.rustTriple}/release/${target.executable}"),
        repoRoot.resolve("target/${target.rustTriple}/debug/${target.executable}"),
    ).firstOrNull { Files.isRegularFile(it) }
}

fun parseWorkspaceVersion(repoRoot: java.nio.file.Path): String? {
    val cargoToml = repoRoot.resolve("Cargo.toml")
    if (!Files.isRegularFile(cargoToml)) return null
    val text = Files.readString(cargoToml)
    val match = Regex("""(?s)\[workspace\.package].*?version\s*=\s*"([^"]+)"""").find(text)
    return match?.groupValues?.get(1)
}

fun localIdeBaselineVersion(idePath: String): Int? {
    val root = Path.of(idePath).toAbsolutePath().normalize()
    val productInfo = listOf(
        root.resolve("product-info.json"),
        root.resolve("Resources/product-info.json"),
        root.resolve("Contents/Resources/product-info.json"),
    ).firstOrNull { Files.isRegularFile(it) } ?: return null
    val buildNumber = Regex(""""buildNumber"\s*:\s*"([^"]+)"""")
        .find(Files.readString(productInfo))
        ?.groupValues
        ?.get(1)
        ?: return null
    return platformBaselineVersion(buildNumber)
}

fun platformBaselineVersion(version: String): Int? {
    Regex("""(?:^|[^0-9])(\d{3})(?:\.|$)""").find(version)?.let { match ->
        return match.groupValues[1].toIntOrNull()
    }
    Regex("""(?:^|[^0-9])(\d{4})\.(\d+)(?:\.|$)""").find(version)?.let { match ->
        val year = match.groupValues[1].toIntOrNull() ?: return null
        val release = match.groupValues[2].toIntOrNull() ?: return null
        return (year - 2000) * 10 + release
    }
    return null
}
