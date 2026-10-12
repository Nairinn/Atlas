// Small Kotlin pieces the three gRPC modules share: Hikari+Exposed
// bootstrap, the gRPC health service wrapper, and SHA-256 hex hashing.
// No protobuf, no shadow jar — this is a library, not a service.

plugins {
    kotlin("jvm")
}

kotlin {
    jvmToolchain(21)
}

dependencies {
    // `api`: these types appear in this module's own public signatures, so
    // consumers need them without re-declaring.
    api(libs.exposed.core)
    api(libs.hikari)
    api(libs.grpc.services)
    api(libs.grpc.protobuf)
    api(libs.protobuf.kotlin)
}

dependencies {
    testImplementation(libs.kotlin.test)
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}
