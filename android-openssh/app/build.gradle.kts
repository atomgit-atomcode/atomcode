plugins {
    id("com.android.application")
}

android {
    namespace = "com.atomcode.tools.openssh"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.atomcode.tools.openssh"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "10.6p1"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    packaging {
        jniLibs {
            // Every one of these is a program the loader must EXTRACT into
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
