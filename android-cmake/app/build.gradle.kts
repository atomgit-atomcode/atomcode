plugins {
    id("com.android.application")
}

android {
    namespace = "com.atomcode.tools.cmake"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.atomcode.tools.cmake"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "4.4.5"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    packaging {
        jniLibs {
            // `libcmake.so` is the driver itself: it must be EXTRACTED into
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
