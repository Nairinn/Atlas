package io.atlas.common

import io.grpc.health.v1.HealthCheckResponse
import io.grpc.protobuf.services.HealthStatusManager

/**
 * Thin wrapper around grpc-services' [HealthStatusManager] so a service
 * entry point can register the standard `grpc.health.v1.Health` service
 * on the server and flip status to NOT_SERVING during graceful shutdown.
 *
 * Kubernetes liveness/readiness probes run `grpc_health_probe` against
 * this service.
 */
class HealthCheck {
    private val manager = HealthStatusManager()

    val service get() = manager.healthService

    fun setServing(serviceName: String = "") {
        manager.setStatus(serviceName, HealthCheckResponse.ServingStatus.SERVING)
    }

    fun setNotServing(serviceName: String = "") {
        manager.setStatus(serviceName, HealthCheckResponse.ServingStatus.NOT_SERVING)
    }

    fun shutdown() {
        manager.enterTerminalState()
    }
}
