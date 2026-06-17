import java.nio.file.Files
import java.nio.file.StandardCopyOption

plugins {
    kotlin("jvm") version "2.2.21"
    kotlin("plugin.serialization") version "2.2.21"
    id("org.jetbrains.intellij.platform") version "2.16.0"
}

group = "com.atomcode"
version = providers.gradleProperty("pluginVersion").get()

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
        val localIdePath = providers.gradleProperty("platformLocalPath")
        if (localIdePath.isPresent) {
            local(localIdePath)
        } else {
            intellijIdeaCommunity(providers.gradleProperty("platformVersion").get())
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
            val localIdePath = providers.gradleProperty("platformLocalPath")
            if (localIdePath.isPresent) {
                local(localIdePath)
            } else {
                recommended()
            }
        }
    }

    signing {
        certificateChain = providers.environmentVariable("JETBRAINS_CERTIFICATE_CHAIN")
        privateKey = providers.environmentVariable("JETBRAINS_PRIVATE_KEY")
        password = providers.environmentVariable("JETBRAINS_PRIVATE_KEY_PASSWORD")
    }

    publishing {
        token = providers.environmentVariable("JETBRAINS_PUBLISH_TOKEN")
        channels = listOf(providers.environmentVariable("JETBRAINS_CHANNEL").orElse("beta").get())
    }
}

tasks {
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

    val bundleDaemon by registering {
        outputs.dir(bundledDaemonDir)
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
