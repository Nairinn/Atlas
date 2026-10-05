package io.atlas.payments.config

/**
 * Runtime configuration assembled from environment variables. Centralized so
 * App.kt has a single place to read env and so missing-but-required vars fail
 * fast at startup rather than mid-request.
 *
 * Defaults match docker-compose.yml so the service "just works" inside the
 * local dev compose stack. Production overrides every value via env.
 */
data class EnvConfig(
    val databaseUrl: String,
    val databaseUser: String,
    val databasePassword: String,
    val kafkaBrokers: String,
    val grpcPort: Int,
    val httpPort: Int,
    val fareTopic: String,
    val outboxPollSeconds: Long,
    val outboxBatchSize: Int,
    /** Which PaymentProvider to run. "fake" until a real processor is wired. */
    val paymentProvider: String,
    /**
     * AES key for control.project_payment_config credential columns.
     * Null when no project uses Stripe — see App.kt.
     */
    val paymentConfigEncKey: String?,
    /** Where Stripe API calls go when overridden (stripe-mock, tests). */
    val stripeApiBase: String?,
) {
    companion object {
        fun fromEnv(env: Map<String, String> = System.getenv()): EnvConfig {
            val rawJdbcUrl = env["DATABASE_URL"]
                ?: "postgres://atlas:atlas_dev@localhost:5432/atlas"
            val (jdbcUrl, user, password) = parseDatabaseUrl(rawJdbcUrl)
            return EnvConfig(
                databaseUrl = jdbcUrl,
                databaseUser = user,
                databasePassword = password,
                kafkaBrokers = env["KAFKA_BROKERS"] ?: "localhost:9092",
                grpcPort = env["GRPC_PORT"]?.toInt() ?: 50053,
                httpPort = env["HTTP_PORT"]?.toInt() ?: 8053,
                fareTopic = env["KAFKA_TOPIC_FARE_EVENTS"] ?: "atlas.fare.events",
                outboxPollSeconds = env["OUTBOX_POLL_SECONDS"]?.toLong() ?: 2,
                outboxBatchSize = env["OUTBOX_BATCH_SIZE"]?.toInt() ?: 100,
                paymentProvider = env["PAYMENT_PROVIDER"] ?: "fake",
                paymentConfigEncKey = env["PAYMENT_CONFIG_ENC_KEY"],
                stripeApiBase = env["STRIPE_API_BASE"],
            )
        }

        // Postgres URLs in compose files use the libpq form
        // `postgres://user:pass@host:port/db`. Hikari needs the JDBC form
        // `jdbc:postgresql://host:port/db` plus separate user/password. This
        // splits the two without dragging in a URI-parsing library.
        private fun parseDatabaseUrl(raw: String): Triple<String, String, String> {
            if (raw.startsWith("jdbc:")) {
                return Triple(raw, "", "")
            }
            val withoutScheme = raw.removePrefix("postgres://").removePrefix("postgresql://")
            val atIdx = withoutScheme.lastIndexOf('@')
            require(atIdx > 0) { "DATABASE_URL must include credentials: $raw" }
            val credentials = withoutScheme.substring(0, atIdx)
            val hostAndPath = withoutScheme.substring(atIdx + 1)
            val colonIdx = credentials.indexOf(':')
            val user = if (colonIdx >= 0) credentials.substring(0, colonIdx) else credentials
            val password = if (colonIdx >= 0) credentials.substring(colonIdx + 1) else ""
            return Triple("jdbc:postgresql://$hostAndPath", user, password)
        }
    }
}
