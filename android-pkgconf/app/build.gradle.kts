plugins {
    id("com.android.application")
}

android {
    namespace = "com.atomcode.tools.pkgconf"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.atomcode.tools.pkgconf"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.29.2"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    packaging {
        jniLibs {
            // The program itself must be EXTRACTED into nativeLibraryDir, the only
            // directory Android executes from.
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
