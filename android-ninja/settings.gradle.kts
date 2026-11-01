// A provider APK that carries one Android-native tool (or a small set of them).
//
// Shared by the ninja/cmake/go providers: a program cannot be executed from this
// app's data directory (measured: exit 126), and the one place Android does
// execute from is some APK's `nativeLibraryDir`. So each tool rides here as
// `lib<stem>.so`, and the app links `<product>/bin/<name>` at it — the same
// carrier as `com.atomcode.tools.zig` and the clang/node providers.
//
// Termux builds these for Android/bionic; `scripts/termux/` fetches and splits
// them, and `scripts/build-android-tool-provider.sh` drives the whole thing.
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

rootProject.name = "AtomCodeToolsNinja"
include(":app")
