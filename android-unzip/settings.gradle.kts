// A provider APK that carries Info-ZIP unzip.
//
// A program cannot be executed from this app's data directory (measured: exit 126),
// and the one place Android executes from is some APK's `nativeLibraryDir`. So the
// binary rides here as `libunzip.so` and the host links `<product>/bin/unzip` at
// it; `libbz2.so.1.0` travels as the `unzip-data` market package.
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

rootProject.name = "AtomCodeToolsUnzip"
include(":app")
