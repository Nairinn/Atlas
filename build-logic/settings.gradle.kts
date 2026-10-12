/*
 * Version catalog is visible to build-logic so the convention plugin can
 * resolve the same plugin versions the modules do.
 */

dependencyResolutionManagement {
    versionCatalogs {
        create("libs") {
            from(files("../gradle/libs.versions.toml"))
        }
    }
}
