// A provider APK that carries rsync.
//
// A program cannot be executed from this app's data directory (measured: exit 126),
// and the one place Android executes from is some APK's `nativeLibraryDir`. So
// rsync rides here as `librsync.so` and the host links `<product>/bin/rsync` at it.
// Its libraries travel as the `rsync-data` market package, because their names carry
// versions Gradle will not package.
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

rootProject.name = "AtomCodeToolsRsync"
include(":app")
