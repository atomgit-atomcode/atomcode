// A provider APK that carries pkg-config.
//
// A `configure` script or a CMake `find_package` looks it up by name, and this app
// may not execute programs from its own data directory (measured: exit 126). The
// one place Android executes from is some APK's `nativeLibraryDir`, so the binary
// rides here as `libpkg-config.so` and the host links `<product>/bin/pkg-config` at
// it — the same carrier as zig/clang/node/ninja/cmake/go.
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

rootProject.name = "AtomCodeToolsPkgconf"
include(":app")
