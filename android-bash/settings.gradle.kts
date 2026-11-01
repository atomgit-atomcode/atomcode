// A provider APK that carries bash.
//
// Android's own `/system/bin/sh` is toybox: enough for the agent's tool calls, not
// for the scripts in a repository — `#!/bin/bash`, `[[ ]]`, arrays, `set -o
// pipefail` all fail there. bash is a program, and a program cannot run from this
// app's data directory (measured: exit 126), so it rides here as `libbash.so` and
// the host links `<product>/bin/bash` at it.
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

rootProject.name = "AtomCodeToolsBash"
include(":app")
