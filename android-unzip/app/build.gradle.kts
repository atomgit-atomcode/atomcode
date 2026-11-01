plugins {
    id("com.android.application")
}

android {
    namespace = "com.atomcode.tools.unzip"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.atomcode.tools.unzip"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "6.0"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    packaging {
        jniLibs {
            // `libunzip.so` is the program itself: it must be EXTRACTED into
            // nativeLibraryDir, the only directory Android executes from.
            useLegacyPackaging = true
        }
    }

    buildTypes {
        getByName("debug") {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
