// Atlas auth-service: passwords, JWTs with atlas:* claims, sessions, a
// 30s token-validation cache, Kafka fanout for cache invalidation, and
// the gRPC server on :50051. Build wiring (protobuf codegen, grpc-kotlin
// stubs, JVM 21, shadow jar) lives in the atlas.kotlin-service
// convention plugin.

plugins {
    id("atlas.kotlin-service")
    application
}

application {
    mainClass.set("io.atlas.auth.AppKt")
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

    // Auth specifics: JWT signing (HS256, jose4j) and bcrypt hashing.
    implementation(libs.jose4j)
    implementation(libs.jbcrypt)

    // gRPC server + generated stubs.
    implementation(libs.grpc.stub)
    implementation(libs.grpc.protobuf)
    implementation(libs.grpc.netty.shaded)
    implementation(libs.grpc.services)
    implementation(libs.grpc.kotlin.stub)
    implementation(libs.protobuf.kotlin)
    implementation(libs.kotlinx.coroutines.core)

    // Kafka producer for atlas.auth.tokens + consumer for cache eviction.
    implementation(libs.kafka.clients)
    implementation(libs.caffeine)

    // Structured logs and /metrics.
    implementation(libs.slf4j.api)
    implementation(libs.logback.classic)
    implementation(libs.logstash.encoder)
    implementation(libs.ktor.server.core)
    implementation(libs.ktor.server.netty)
    implementation(libs.micrometer.registry.prometheus)

    testImplementation(libs.kotlin.test)
    testImplementation(libs.junit.jupiter)
    testImplementation(libs.kotlinx.coroutines.test)
    testRuntimeOnly(libs.junit.platform.launcher)
}

tasks.shadowJar {
    archiveBaseName.set("auth-service")
}
