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

    buildTypes {
        release {
            isMinifyEnabled = false
            // The Rust library is prebuilt by `scripts/build-android.sh`; Gradle
            // packages whatever is in `jniLibs/arm64-v8a/`.
            ndk { debugSymbolLevel = "none" }
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