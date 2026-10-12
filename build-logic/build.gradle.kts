/*
 * Convention plugins for the Atlas Kotlin services.
 *
 * One precompiled script plugin, `atlas.kotlin-service`, wires everything
 * the three gRPC/Kotlin modules (auth-service, payments-service,
 * fare-consumer) share: JVM 21 toolchain, protobuf codegen from the
 * repo-root proto/, grpc + grpc-kotlin generators, and the shadow fat
 * jar the Dockerfiles run.
 *
 * Precompiled script plugins cannot see the version catalog directly, so
 * the protobuf artifact coordinates (the only version-specific values the
 * convention needs) are bridged in by generating a small Kotlin object
 * from the catalog — the versions still live in exactly one place.
 */

plugins {
    `kotlin-dsl`
}

repositories {
    gradlePluginPortal()
    mavenCentral()
}

dependencies {
    // The plugin ARTIFACTS (not markers): a precompiled script plugin that
    // applies these needs them on build-logic's classpath.
    implementation(libs.kotlin.gradle.plugin)
    implementation(libs.protobuf.gradle.plugin)
    implementation(libs.shadow.gradle.plugin)
}

val generatedVersions = layout.buildDirectory.dir("generated/versions")

val generateCatalogBridge by tasks.registering {
    val protoc = libs.protoc.get().toString()
    val grpcJava = libs.grpc.codegen.java.get().toString()
    val grpcKotlin = libs.grpc.codegen.kotlin.get().toString()
    inputs.property("protoc", protoc)
    inputs.property("grpcJava", grpcJava)
    inputs.property("grpcKotlin", grpcKotlin)
    outputs.dir(generatedVersions)
    doLast {
        generatedVersions.get().file("AtlasProtoArtifacts.kt").asFile.writeText(
            """
            // GENERATED from gradle/libs.versions.toml — do not edit.

            /** Artifact coordinates the protobuf codegen wiring needs. */
            object AtlasProtoArtifacts {
                const val PROTOC: String = "$protoc"
                const val GRPC_JAVA: String = "$grpcJava"
                const val GRPC_KOTLIN: String = "$grpcKotlin"
            }
            """.trimIndent()
        )
    }
}

// The bridge file lands in the convention plugin's compiled classpath.
sourceSets {
    main {
        kotlin.srcDir(generatedVersions)
    }
}

tasks.named("compileKotlin") {
    dependsOn(generateCatalogBridge)
}
