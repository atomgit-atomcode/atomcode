// A provider APK that carries the cmake driver.
//
// cmake is a program, and this app may not execute programs from its own data
// directory (measured: exit 126). The one place Android executes from is some
// APK's `nativeLibraryDir`, so the driver rides here as `libcmake.so` and the host
// links `<product>/bin/cmake` at it — the same carrier as zig/clang/node.
//
// The module set (`share/cmake-4.4`) is read-only and ships as the `cmake-data`
// market package; `scripts/termux/` fetches and splits everything.
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

rootProject.name = "AtomCodeToolsCmake"
include(":app")
