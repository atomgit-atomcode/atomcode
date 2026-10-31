// A provider APK that carries exactly one native binary: `zig`.
//
// The main app cannot execute anything from its own data directory (measured: exit
// 126 from the real app process), and a market *data* package is the same directory,
// so a process-model tool can only live in some APK's `nativeLibraryDir`. Splitting
// zig out of the `com.atomcode.tools` helper is what took that APK from 82 MB to a
// few MB — zig alone was 141 MB unpacked.
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

rootProject.name = "AtomCodeToolsZig"
include(":app")
