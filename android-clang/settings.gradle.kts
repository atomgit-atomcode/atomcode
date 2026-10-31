// A provider APK that carries an Android/bionic LLVM toolchain: clang, clang++ and lld.
//
// Nothing here can run from the app's data directory (measured: exit 126 from the
// real app process), so every executable lives in this package's
// `nativeLibraryDir` — the one place the platform executes from — and the main app
// links `<data>/bin/clang` at it. The headers and the clang resource directory are
// *read*, not executed, so they are a market data package instead of APK weight.
//
// Where it comes from: Termux publishes maintained bionic builds of LLVM. The
// scripts under `scripts/termux/` fetch, split and package them; nothing in this
// project is hand-copied.
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "AtomCodeToolsClang"
include(":app")