import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

/// The product version, read from `[workspace.package] version` in the workspace
/// `Cargo.toml` — the same number the CLI prints.
///
/// It used to be typed here, and it drifted: the APK advertised 5.2.5 while the
/// daemon inside reported 5.2.6, so a store listing (and the `APK 5.2.5` line a
/// person compares against `atomcode --version`) disagreed with the product.
/// Read it instead of repeating it.
fun workspaceVersion(): String {
    val manifest = rootProject.file("../Cargo.toml")
    require(manifest.isFile) { "workspace Cargo.toml not found at ${manifest.absolutePath}" }
    var inPackage = false
    for (line in manifest.readLines()) {
        val text = line.trim()
        if (text.startsWith("[")) {
            inPackage = text == "[workspace.package]"
            continue
        }
        if (inPackage && text.startsWith("version")) {
            return text.substringAfter('=').trim().removeSurrounding("\"")
        }
    }
    error("no `version` under [workspace.package] in ${manifest.absolutePath}")
}

/// `major * 10000 + minor * 100 + patch` — a single monotonically increasing
/// integer, which is all Android asks of `versionCode`. A pre-release suffix
/// (`-beta.1`) is dropped: it would make the code non-monotonic.
fun versionCodeOf(version: String): Int {
    val digits = version.takeWhile { it.isDigit() || it == '.' }
    val parts = digits.split('.').map { it.toInt() }
    require(parts.size >= 3) { "version `$version` is not major.minor.patch" }
    return parts[0] * 10000 + parts[1] * 100 + parts[2]
}

val productVersion = workspaceVersion()
val productVersionCode = versionCodeOf(productVersion)

/// Release signing material, resolved from the machine — never from the repo.
///
/// Four settings, each with an environment variable that wins over
/// `local.properties` so CI can keep the material out of any checked-in file:
///
/// | local.properties | environment |
/// |---|---|
/// | `atomcode.keystore` | `ATOMCODE_ANDROID_KEYSTORE` |
/// | `atomcode.keystore.password` | `ATOMCODE_ANDROID_KEYSTORE_PASSWORD` |
/// | `atomcode.key.alias` | `ATOMCODE_ANDROID_KEY_ALIAS` |
/// | `atomcode.key.password` | `ATOMCODE_ANDROID_KEY_PASSWORD` |
///
/// Nothing is defaulted and no key is generated here: a release key identifies
/// the app for its whole life, so producing one is a person's decision, not a
/// build step's. Absent the material `assembleRelease` still runs and stays
/// unsigned; `scripts/verify-android-release.sh` refuses that output instead of
/// letting an unsigned artifact reach a store upload.
data class ReleaseSigning(
    val keystore: File,
    val storePassword: String,
    val keyAlias: String,
    val keyPassword: String,
)

fun machineProperties(): Properties {
    val file = rootProject.file("local.properties")
    return Properties().apply { if (file.isFile) file.inputStream().use { load(it) } }
}

val localProperties = machineProperties()

fun signingSetting(key: String, env: String): String? =
    System.getenv(env)?.trim()?.takeIf { it.isNotEmpty() }
        ?: localProperties.getProperty(key)?.trim()?.takeIf { it.isNotEmpty() }

val releaseSigning: ReleaseSigning? = run {
    val keystore = signingSetting("atomcode.keystore", "ATOMCODE_ANDROID_KEYSTORE") ?: return@run null
    val storePassword = signingSetting("atomcode.keystore.password", "ATOMCODE_ANDROID_KEYSTORE_PASSWORD")
    val alias = signingSetting("atomcode.key.alias", "ATOMCODE_ANDROID_KEY_ALIAS")
    val keyPassword = signingSetting("atomcode.key.password", "ATOMCODE_ANDROID_KEY_PASSWORD")
    // Half-configured is worse than absent: without this the build fails deep
    // inside `apksigner` with a stack trace that never names the missing key.
    val missing = buildList {
        if (storePassword == null) add("atomcode.keystore.password")
        if (alias == null) add("atomcode.key.alias")
        if (keyPassword == null) add("atomcode.key.password")
    }
    require(missing.isEmpty()) {
        "release keystore is set but ${missing.joinToString(", ")} is missing " +
            "(local.properties, or the ATOMCODE_ANDROID_KEYSTORE* environment variables)"
    }
    val file = rootProject.file(keystore)
    require(file.isFile) { "release keystore not found: ${file.absolutePath}" }
    ReleaseSigning(file, storePassword!!, alias!!, keyPassword!!)
}

if (releaseSigning == null) {
    logger.warn(
        "release signing is not configured (atomcode.keystore in local.properties or " +
            "ATOMCODE_ANDROID_KEYSTORE* in the environment): assembleRelease will produce an UNSIGNED apk",
    )
}

android {
    namespace = "com.atomcode.app"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.atomcode.app"
        // API 26 is the floor for a foreground service with a notification
        // channel; the WebView on anything older is too old for the webui anyway.
        minSdk = 26
        targetSdk = 36
        // Tracks the product version, which lives in the workspace Cargo.toml
        // and is the same number the CLI prints. It used to sit at a stray
        // "0.1.0"/1, which meant the APK could neither be recognised as newer
        // nor be installed over a build of the same version.
        //
        // Both come from `Cargo.toml` (see `workspaceVersion` above): the
        // product version 5.2.6 must not have to be retyped here, because the
        // release that forgot to retype it shipped an APK advertising 5.2.5.
        versionCode = productVersionCode
        versionName = productVersion

        ndk {
            // One ABI. The Rust core is ~40–60 MB per architecture; shipping
            // four would quadruple the APK for hardware nobody runs this on.
            abiFilters += "arm64-v8a"
        }
    }

    signingConfigs {
        if (releaseSigning != null) {
            create("release") {
                storeFile = releaseSigning.keystore
                storePassword = releaseSigning.storePassword
                keyAlias = releaseSigning.keyAlias
                keyPassword = releaseSigning.keyPassword
                // v1 too: minSdk is 26 and a sideloaded APK still goes through the
                // JAR signature on some installers. v2/v3 are what a store checks.
                enableV1Signing = true
                enableV2Signing = true
                enableV3Signing = true
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // The Rust library is prebuilt by `scripts/build-android.sh`; Gradle
            // packages whatever is in `jniLibs/arm64-v8a/`.
            ndk { debugSymbolLevel = "none" }
            // Machine-local, see `releaseSigning` above. Without it this is an
            // unsigned release apk — installable nowhere, uploadable nowhere.
            signingConfig = signingConfigs.findByName("release")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    packaging {
        jniLibs {
            // Extracted into the APK's native library dir. `true` is required,
            // not stylistic: the bundled `git` is installed as `libgit.so` and
            // executed from that dir (Android 10+ forbids exec anywhere else an
            // app can write). With `false` the libraries stay inside the APK and
            // `nativeLibraryDir` has no file to exec.
            useLegacyPackaging = true
        }
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-ktx:1.9.3")
    implementation("androidx.webkit:webkit:1.12.1")
}