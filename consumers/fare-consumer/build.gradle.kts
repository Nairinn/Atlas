// Atlas fare-consumer.
//
// Phase 6 scope:
//   - Consumes atlas.fare.events (protobuf FareEvent from proto/events.proto)
//   - Drives settlement: RIDE_COMPLETED -> SettleTransaction,
//     RIDE_CANCELLED -> RefundTransaction, both over gRPC to
//     payments-service. Money is never moved by writing to the database
//     directly.
//   - Writes the payments.transaction_events audit log that migration 0030
//     reserved for this consumer, deduplicated on event_key (0032).
//   - /metrics via Micrometer on its own Ktor server.
//
// It needs the protobuf plugin for two things: the FareEvent message it
// decodes, and the PaymentsService client stub it calls.

import com.google.protobuf.gradle.id

plugins {
    alias(libs.plugins.kotlin.jvm)
    alias(libs.plugins.protobuf)
    alias(libs.plugins.shadow)
    application
}

kotlin {
    jvmToolchain(21)
}

application {
    mainClass.set("io.atlas.fare.AppKt")
}

sourceSets {
    main {
        java.srcDirs(
            "build/generated/source/proto/main/java",
            "build/generated/source/proto/main/grpc",
            "build/generated/source/proto/main/grpckt",
            "build/generated/source/proto/main/kotlin",
        )
        proto {
            srcDir("$rootDir/proto")
        }
    }
}

protobuf {
    protoc {
        artifact = libs.protoc.get().toString()
    }
    plugins {
        id("grpc") {
            artifact = libs.grpc.codegen.java.get().toString()
        }
        id("grpckt") {
            artifact = libs.grpc.codegen.kotlin.get().toString() + ":jdk8@jar"
        }
    }
    generateProtoTasks {
        all().forEach { task ->
            task.plugins {
                id("grpc") {}
                id("grpckt") {}
            }
            task.builtins {
                id("kotlin") {}
            }
        }
    }
}

dependencies {
    implementation(libs.exposed.core)
    implementation(libs.exposed.jdbc)
    implementation(libs.exposed.java.time)
    implementation(libs.hikari)
    implementation(libs.postgres)
    implementation(libs.slf4j.api)
    implementation(libs.logback.classic)
    implementation(libs.logstash.encoder)

    implementation(libs.grpc.stub)
    implementation(libs.grpc.protobuf)
    implementation(libs.grpc.netty.shaded)
    implementation(libs.grpc.kotlin.stub)
    implementation(libs.protobuf.kotlin)
    implementation(libs.kotlinx.coroutines.core)
    implementation(libs.kafka.clients)

    implementation(libs.ktor.server.core)
    implementation(libs.ktor.server.netty)
    implementation(libs.micrometer.registry.prometheus)

    testImplementation(libs.kotlin.test)
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

tasks.shadowJar {
    archiveBaseName.set("fare-consumer")
    archiveClassifier.set("all")
    archiveVersion.set("")
    mergeServiceFiles()
}
