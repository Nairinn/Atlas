/*
 * Root Gradle settings for the three Kotlin modules. Repositories are
 * declared ONLY here (dependencyResolutionManagement below); modules get
 * them transitively and declare dependencies through the libs catalog.
 */

rootProject.name = "atlas"

// Precompiled convention plugins for the Kotlin modules.
includeBuild("build-logic")

// Phase 2
include("services:auth-service")
// Phase 4
include("services:payments-service")
// Phase 6
include("consumers:fare-consumer")
// Shared Kotlin library for the three gRPC modules.
include("libs:atlas-common")

pluginManagement {
    repositories {
        gradlePluginPortal()
        mavenCentral()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.PREFER_PROJECT)
    repositories {
        mavenCentral()
    }
}
