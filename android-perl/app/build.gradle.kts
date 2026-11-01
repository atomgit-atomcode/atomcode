plugins {
    id("com.android.application")
}

android {
    namespace = "com.atomcode.tools.perl"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.atomcode.tools.perl"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "5.44.0"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    packaging {
        jniLibs {
            // Both the driver and libperl.so must be EXTRACTED into
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
