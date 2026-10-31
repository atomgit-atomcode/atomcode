plugins {
    id("com.android.application")
}

android {
    namespace = "com.atomcode.tools.zig"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.atomcode.tools.zig"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.13.0"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    packaging {
        jniLibs {
            // Same reason as the helper: the binary must be EXTRACTED into
            // nativeLibraryDir, the only place Android executes from.
            useLegacyPackaging = true
        }
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
        }
    }
}
