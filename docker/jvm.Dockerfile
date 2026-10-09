# All three JVM images (auth-service, payments-service, fare-consumer)
# build from this one file.
#
#   docker build -f docker/jvm.Dockerfile \
#     --build-arg DIR=services/auth-service \
#     --build-arg TASK=services:auth-service \
#     --build-arg JAR=auth-service .
#
# DIR is the filesystem path (used by COPY); TASK is the Gradle project
# path (colon-separated, used by task selectors). They are separate args
# because Gradle paths and directory paths are written differently and
# conflating them is how a ${DIR}:shadowJar selector silently breaks.
# Build context must be the repo root: the Gradle build pulls .proto files
# from $rootDir/proto.

FROM gradle:8.10.2-jdk21 AS builder
ARG DIR
ARG TASK
ARG JAR
WORKDIR /build

# Build configuration first so dependency resolution caches across
# source-only changes. The `help` task warms the cache without needing
# sources; `|| true` because an empty-project warning must not fail the
# image build.
COPY settings.gradle.kts build.gradle.kts gradle.properties* ./
COPY gradle ./gradle
COPY ${DIR}/build.gradle.kts ./${DIR}/build.gradle.kts
RUN gradle --no-daemon :${TASK}:help -q || true

# Sources and the shared proto/ directory, then the fat jar.
COPY proto ./proto
COPY ${DIR}/src ./${DIR}/src
RUN gradle --no-daemon :${TASK}:shadowJar

FROM eclipse-temurin:21-jre AS runtime
# ARGs do not cross FROM boundaries; re-declared for the COPY.
ARG DIR
ARG JAR
WORKDIR /app
COPY --from=builder /build/${DIR}/build/libs/${JAR}-all.jar /app/service.jar
# Non-root: matches the Rust images. The JVM needs no /etc/passwd entry to
# run as a bare numeric uid.
USER 10001
ENTRYPOINT ["java", "-jar", "/app/service.jar"]
