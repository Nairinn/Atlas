import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math';

import 'errors.dart';

const String keyHeader = 'X-Atlas-Key';

/// The transport: headers, retries, timeouts.
///
/// Built on `dart:io`'s HttpClient rather than package:http so the SDK has
/// no runtime dependencies — a package that pulls in `http` forces that
/// version choice on every Flutter app that adopts it.
class Http {
  Http({
    required String baseUrl,
    required this.projectKey,
    required this.timeout,
    required this.maxRetries,
    HttpClient? client,
  })  : baseUrl = baseUrl.endsWith('/')
            ? baseUrl.substring(0, baseUrl.length - 1)
            : baseUrl,
        _client = client ?? HttpClient();

  final String baseUrl;
  final String projectKey;
  final Duration timeout;
  final int maxRetries;
  final HttpClient _client;
  final Random _random = Random();

  String? token;

  void close() => _client.close(force: true);

  /// Send a request, retrying only when it is safe to.
  ///
  /// [replayable] marks a POST the caller has made idempotent by
  /// supplying an idempotency key. Without it a POST is sent exactly once:
  /// replaying a non-idempotent write is how one deposit becomes two.
  Future<dynamic> send({
    required String method,
    required String path,
    Map<String, dynamic>? body,
    Map<String, String>? query,
    bool authenticated = true,
    bool replayable = false,
  }) async {
    final idempotent = method == 'GET' || method == 'DELETE' || replayable;
    final attempts = idempotent ? maxRetries + 1 : 1;

    Object? lastError;
    int? retryAfterMs;
    for (var attempt = 0; attempt < attempts; attempt++) {
      if (attempt > 0) {
        // Exponential backoff with jitter. Without jitter every client
        // that failed together retries together, and the recovering
        // service is hit by a synchronised wave. A Retry-After the
        // server sent (429) overrides the guess.
        final base = 100 * (1 << (attempt - 1));
        final wait = (retryAfterMs ?? 0) > base ? retryAfterMs! : base;
        await Future<void>.delayed(
          Duration(milliseconds: wait + _random.nextInt(base ~/ 2 + 1)),
        );
        retryAfterMs = null;
      }

      try {
        return await _attempt(
          method: method,
          path: path,
          body: body,
          query: query,
          authenticated: authenticated,
        );
      } on AtlasError catch (e) {
        if (!e.isRetryable || attempt + 1 == attempts) rethrow;
        retryAfterMs = e.retryAfterMs;
        lastError = e;
      } on AtlasConnectionError catch (e) {
        if (attempt + 1 == attempts) rethrow;
        lastError = e;
      }
    }
    throw lastError ?? AtlasConnectionError('no attempt was made');
  }

  Future<dynamic> _attempt({
    required String method,
    required String path,
    Map<String, dynamic>? body,
    Map<String, String>? query,
    required bool authenticated,
  }) async {
    final uri = Uri.parse('$baseUrl$path').replace(
      queryParameters: (query == null || query.isEmpty) ? null : query,
    );

    HttpClientResponse response;
    String text;
    // One overall deadline for the whole attempt. Timing each phase
    // separately would allow up to 3x the configured timeout for a
    // single request, which is not what a caller setting `timeout` means.
    final deadline = DateTime.now().add(timeout);
    try {
      final request = await _client
          .openUrl(method, uri)
          .timeout(timeout, onTimeout: () => throw TimeoutException('connect'));

      // Sent on every request, including register and login: creating a
      // user means creating them in a project.
      request.headers.set(keyHeader, projectKey);
      if (authenticated && token != null) {
        request.headers.set(HttpHeaders.authorizationHeader, 'Bearer $token');
      }
      if (body != null) {
        request.headers.contentType = ContentType.json;
        request.write(jsonEncode(body));
      }

      response =
          await request.close().timeout(deadline.difference(DateTime.now()));
      text = await response
          .transform(utf8.decoder)
          .join()
          .timeout(deadline.difference(DateTime.now()));
    } on TimeoutException {
      throw AtlasConnectionError('request to $uri timed out after $timeout');
    } on SocketException catch (e) {
      throw AtlasConnectionError('could not reach $uri: ${e.message}');
    } on HttpException catch (e) {
      throw AtlasConnectionError('could not reach $uri: ${e.message}');
    }

    if (response.statusCode >= 200 && response.statusCode < 300) {
      if (text.trim().isEmpty) return null;
      try {
        return jsonDecode(text);
      } on FormatException catch (e) {
        // A 2xx the SDK cannot parse is the server's bug, not the
        // network's: report it as a decode error so callers can tell the
        // difference from a connection failure.
        throw AtlasDecodeError(
            'malformed JSON in response from $path: ${e.message}');
      }
    }
    throw _toError(
      response.statusCode,
      text,
      retryAfterMs: _parseRetryAfterMs(
        response.headers.value('retry-after'),
      ),
    );
  }

  /// Seconds form only, capped at 30s so a hostile or buggy value cannot
  /// stall a caller indefinitely. The HTTP-date form is ignored: Atlas's
  /// own limiters send seconds, and a misread hint degrades to the normal
  /// backoff rather than a wrong wait.
  static const _maxRetryAfterMs = 30000;

  int? _parseRetryAfterMs(String? header) {
    if (header == null) return null;
    final secs = int.tryParse(header.trim());
    if (secs == null) return null;
    if (secs <= 0) return 0;
    return secs * 1000 > _maxRetryAfterMs ? _maxRetryAfterMs : secs * 1000;
  }

  AtlasError _toError(int status, String body, {int? retryAfterMs}) {
    try {
      final decoded = jsonDecode(body);
      if (decoded is Map<String, dynamic>) {
        final envelope = decoded['error'];
        if (envelope is Map<String, dynamic>) {
          return AtlasError(
            code: AtlasErrorCode.fromWire(envelope['code'] as String?),
            message: envelope['message'] as String? ?? 'unknown error',
            status: status,
            retryAfterMs: retryAfterMs,
          );
        }
      }
    } on FormatException {
      // Falls through: a non-JSON body means something between the caller
      // and the gateway answered — a proxy, an ingress 502 — and the
      // status is the only reliable signal.
    }
    return AtlasError(
      code: _codeForStatus(status),
      message: body.isEmpty
          ? 'HTTP $status'
          : body.substring(0, body.length < 200 ? body.length : 200),
      status: status,
      retryAfterMs: retryAfterMs,
    );
  }

  AtlasErrorCode _codeForStatus(int status) => switch (status) {
        401 => AtlasErrorCode.unauthenticated,
        403 => AtlasErrorCode.permissionDenied,
        404 => AtlasErrorCode.notFound,
        409 => AtlasErrorCode.alreadyExists,
        429 => AtlasErrorCode.resourceExhausted,
        502 || 503 || 504 => AtlasErrorCode.unavailable,
        _ => AtlasErrorCode.unknown,
      };
}
