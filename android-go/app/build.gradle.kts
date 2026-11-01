plugins {
    id("com.android.application")
}

android {
    namespace = "com.atomcode.tools.go"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.atomcode.tools.go"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "1.27.3"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    packaging {
        jniLibs {
            // `libgo.so` is the driver itself, and every `lib<tool>.so` is a
            // program the driver execs: they must be EXTRACTED into
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
