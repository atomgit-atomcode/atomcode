// A provider APK that carries the OpenSSH client.
//
// `ssh`, `scp` and `sftp` are programs, and a program cannot run from this app's
// data directory (measured: exit 126). They ride here as `lib<name>.so` and the host
// links `<product>/bin/<name>` at them; the shared libraries they need travel as
// the `openssh-data` market package.
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

rootProject.name = "AtomCodeToolsOpenssh"
include(":app")
