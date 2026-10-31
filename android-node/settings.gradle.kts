// A provider APK that carries the node interpreter.
//
// The current `node` market package embeds nodejs-mobile's libnode through
// pluginhost. That stops at node 18.20.4 (its last release), so an upgrade needs a
// different shape: node itself is a program, and this app may not execute programs
// from its own data directory (measured: exit 126). The one place Android does
// execute from is some APK's `nativeLibraryDir`, which is what this package is —
// the same carrier as `com.atomcode.tools.zig`.
//
// Termux builds node for Android/bionic; `scripts/termux/` fetches and splits it.
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

rootProject.name = "AtomCodeToolsNode"
include(":app")