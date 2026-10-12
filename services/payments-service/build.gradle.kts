// Atlas payments-service: wallet ledger, transactions with idempotency
// keys, the transactional outbox, the Stripe Connect adapter, and the
// gRPC server on :50053. Webhooks and /metrics ride a small Ktor server
// on :8053. Build wiring (protobuf codegen, grpc-kotlin stubs, JVM 21,
// shadow jar) lives in the atlas.kotlin-service convention plugin.

plugins {
    id("atlas.kotlin-service")
    application
}

application {
    mainClass.set("io.atlas.payments.AppKt")
}

dependencies {
    // Shared bootstrap/health/hashing library.
    implementation(project(":libs:atlas-common"))

    // Storage: Exposed over Postgres with a Hikari pool.
    implementation(libs.exposed.core)
    implementation(libs.exposed.jdbc)
    implementation(libs.exposed.java.time)
    implementation(libs.hikari)
    implementation(libs.postgres)

    // gRPC + protobuf.
    implementation(libs.grpc.stub)
    implementation(libs.grpc.protobuf)
    implementation(libs.grpc.netty.shaded)
    implementation(libs.grpc.services)
    implementation(libs.grpc.kotlin.stub)
    implementation(libs.protobuf.kotlin)
    implementation(libs.kotlinx.coroutines.core)
    implementation(libs.kafka.clients)

    // Ktor HTTP (metrics + provider webhooks) and Micrometer Prometheus.
    implementation(libs.ktor.server.core)
    implementation(libs.ktor.server.netty)
    implementation(libs.micrometer.registry.prometheus)

    // Stripe Connect adapter (per-project processor accounts).
    implementation(libs.stripe.java)

    implementation(libs.slf4j.api)
    implementation(libs.logback.classic)
    implementation(libs.logstash.encoder)

    testImplementation(libs.kotlin.test)
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

tasks.shadowJar {
    archiveBaseName.set("payments-service")
}
