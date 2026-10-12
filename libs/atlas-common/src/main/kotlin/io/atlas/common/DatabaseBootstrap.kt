package io.atlas.common

import com.zaxxer.hikari.HikariConfig
import com.zaxxer.hikari.HikariDataSource
import org.jetbrains.exposed.sql.Database

/**
 * Hikari + Exposed bootstrap. Called once at startup by each service's
 * entry point. Kept tiny on purpose: configuration knobs that have
 * actually been needed are exposed; everything else relies on
 * Hikari/Exposed defaults.
 */
object DatabaseBootstrap {
    fun connect(
        jdbcUrl: String,
        username: String,
        password: String,
        maximumPoolSize: Int = 10,
        poolName: String,
    ): Database {
        val config = HikariConfig().apply {
            this.jdbcUrl = jdbcUrl
            this.username = username
            this.password = password
            this.maximumPoolSize = maximumPoolSize
            this.driverClassName = "org.postgresql.Driver"
            // Exposed manages its own transactions.
            this.isAutoCommit = false
            this.poolName = poolName
        }
        val dataSource = HikariDataSource(config)
        return Database.connect(dataSource)
    }
}
