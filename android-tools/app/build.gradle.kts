plugins {
    id("com.android.application")
}

android {
    namespace = "com.atomcode.tools"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.atomcode.tools"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "1.0"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    packaging {
        jniLibs {
            // The point of the probe: the binary must be EXTRACTED into
            // nativeLibraryDir, which is the only place Android executes from.
            useLegacyPackaging = true
        }
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
        }
    }
}
