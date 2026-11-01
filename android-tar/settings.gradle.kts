// A provider APK that carries GNU tar.
//
// The platform ships toybox's `tar`; build and release scripts want GNU's flags
// (`--transform`, `--exclude-from`, sparse files, archive auto-detection). A
// program cannot run from this app's data directory (measured: exit 126), so the
// binary rides here as `libtar.so` and the host links `<product>/bin/tar` at it —
// its libraries travel as the `tar-data` market package.
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

rootProject.name = "AtomCodeToolsTar"
include(":app")
