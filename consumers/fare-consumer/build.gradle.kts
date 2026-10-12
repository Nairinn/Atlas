// Atlas fare-consumer: consumes atlas.fare.events, drives settlement
// (RIDE_COMPLETED -> settle, RIDE_CANCELLED -> cancel/void) over gRPC to
// payments-service, and writes the transaction_events audit log. Money
// is never moved by writing to the database directly. Build wiring
// (protobuf codegen for the FareEvent it decodes and the PaymentsService
// stub it calls, grpc-kotlin, JVM 21, shadow jar) lives in the
// atlas.kotlin-service convention plugin.

plugins {
    id("atlas.kotlin-service")
    application
}

application {
    mainClass.set("io.atlas.fare.AppKt")
}

dependencies {
    // Shared bootstrap library.
    implementation(project(":libs:atlas-common"))

    // Storage: Exposed over Postgres with a Hikari pool.
    implementation(libs.exposed.core)
    implementation(libs.exposed.jdbc)
    implementation(libs.exposed.java.time)
    implementation(libs.hikari)
    implementation(libs.postgres)

    implementation(libs.grpc.stub)
    implementation(libs.grpc.protobuf)
    implementation(libs.grpc.netty.shaded)
    implementation(libs.grpc.kotlin.stub)
    implementation(libs.protobuf.kotlin)
    implementation(libs.kotlinx.coroutines.core)
    implementation(libs.kafka.clients)

    // /metrics via Micrometer on its own Ktor server.
    implementation(libs.ktor.server.core)
    implementation(libs.ktor.server.netty)
    implementation(libs.micrometer.registry.prometheus)

    implementation(libs.slf4j.api)
    implementation(libs.logback.classic)
    implementation(libs.logstash.encoder)

    testImplementation(libs.kotlin.test)
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

tasks.shadowJar {
    archiveBaseName.set("fare-consumer")
}
