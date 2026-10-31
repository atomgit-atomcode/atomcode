plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
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
        // `versionCode` must be a single monotonically increasing integer:
        // major * 10000 + minor * 100 + patch, so 5.2.5 → 50205. Bump both with
        // every release; forgetting versionCode makes Android refuse the update.
        versionCode = 50205
        versionName = "5.2.5"

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