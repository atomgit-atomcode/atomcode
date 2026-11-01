// A provider APK that carries perl.
//
// A program cannot run from this app's data directory (measured: exit 126), so the
// interpreter rides here: `libperl-driver.so` is `bin/perl` (a thin binary that
// links `libperl.so`, also here) and the host links `<product>/bin/perl` at it.
//
// The module tree (`lib/perl5/5.44.0`) is read, not executed, and is 67 MB — it
// travels as the `perl-data` market package, with the host setting `PERL5LIB`
// because perl's built-in `@INC` points at Termux's own prefix.
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

rootProject.name = "AtomCodeToolsPerl"
include(":app")
