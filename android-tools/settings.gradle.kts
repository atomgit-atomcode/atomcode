// Throwaway probe: a second APK that carries one native binary, so the main app
// can test whether Android lets it execute ANOTHER package's nativeLibraryDir.
// If it does, a helper APK is just a file holder and needs no service or IPC.
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

rootProject.name = "AtomCodeTools"
include(":app")
