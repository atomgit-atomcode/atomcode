plugins {
    id("com.android.application")
}

android {
    namespace = "com.atomcode.tools.clang"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.atomcode.tools.clang"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "21.1.8"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    packaging {
        jniLibs {
            // `scripts/build-android-clang-provider.sh` fills jniLibs with
            // `libclang.so`, `liblld.so`, `libLLVM.so`, … — real ELF files that are
            // also the *executables*. They must be EXTRACTED into nativeLibraryDir
            // (legacy packaging), because a compressed entry inside the APK cannot
            // be exec'd and the zip path is not a filesystem path.
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