// A provider APK that carries the Go toolchain driver and its tools.
//
// Go is a program, and this app may not execute programs from its own data
// directory (measured: exit 126). The one place Android executes from is some
// APK's `nativeLibraryDir`, so the driver rides here as `libgo.so` and each
// toolchain tool as `lib<tool>.so`; the host links them into the GOROOT tree as
// symlinks, which keeps the executable inode inside this APK.
//
// The standard library sources the driver compiles against are read-only and
// ship as the `go-data` market package; `scripts/termux/` fetches and splits.
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

rootProject.name = "AtomCodeToolsGo"
include(":app")
