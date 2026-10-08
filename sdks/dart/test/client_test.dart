// Tests against a real HTTP server on a real port.
//
// Not a mocked transport: the point is to exercise the actual request path
// — headers, query encoding, retries, JSON handling — the way it runs in
// production. A mock only proves the SDK can call a mock.

import 'dart:convert';
import 'dart:io';

import 'package:atlas_sdk/atlas_sdk.dart';
import 'package:test/test.dart';

const testKey = 'atl_test_0123456789abcdef0123456789abcdef';

/// What the fake gateway saw, so tests can assert on the wire.
class Recorded {
  Recorded(this.method, this.path, this.query, this.key, this.authorization,
      this.body);

  final String method;
  final String path;
  final Map<String, String> query;
  final String? key;
  final String? authorization;
  final String body;
}

class FakeGateway {
  FakeGateway._(this._server);

  static Future<FakeGateway> start() async {
    final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
    final gateway = FakeGateway._(server);
    gateway._serve();
    return gateway;
  }

  final HttpServer _server;
  final List<Recorded> requests = [];

  /// Fail this many times before succeeding, for the retry tests.
  int failTimes = 0;
  int attempts = 0;

  /// Answer 429 + Retry-After this many times, for the rate-limit test.
  int rateLimitedTimes = 0;

  String get baseUrl => 'http://127.0.0.1:${_server.port}';

  Future<void> close() => _server.close(force: true);

  void _serve() {
    _server.listen((request) async {
      final body = await utf8.decoder.bind(request).join();
      requests.add(Recorded(
        request.method,
        request.uri.path,
        request.uri.queryParameters,
        request.headers.value('x-atlas-key'),
        request.headers.value('authorization'),
        body,
      ));

      request.response.headers.contentType = ContentType.json;

      // Endpoints that participate in the retry tests.
      if (request.uri.path == '/v1/payments/wallet' ||
          request.uri.path == '/v1/payments/deposits') {
        final seen = attempts++;
        if (seen < rateLimitedTimes) {
          request.response.statusCode = 429;
          request.response.headers.set('retry-after', '1');
          request.response.write(jsonEncode({
            'error': {
              'code': 'resource_exhausted',
              'message': 'rate limit exceeded'
            }
          }));
          await request.response.close();
          return;
        }
        if (seen < failTimes) {
          request.response.statusCode = 503;
          request.response.write(jsonEncode({
            'error': {'code': 'unavailable', 'message': 'try again'}
          }));
          await request.response.close();
          return;
        }
      }

      final payload = switch (request.uri.path) {
        '/v1/auth/register' => {'user_id': 'u-1'},
        '/v1/auth/login' => {'token': 'tok-abc', 'expires_at': 1787000000},
        '/v1/auth/logout' => {'success': true},
        '/v1/auth/me' => {
            'user_id': 'u-1',
            'session_id': 's-1',
            'last_lat': 0.0,
            'last_lng': 0.0,
            'issued_at': 1,
            'expires_at': 2,
            'email': 'a@b.dev',
            'email_verified_at': null,
            'created_at': 1,
          },
        '/v1/auth/password-reset' => {'accepted': true},
        '/v1/auth/password-reset/confirm' => {'user_id': 'u-1'},
        '/v1/auth/email/verify/confirm' => {'user_id': 'u-1'},
        '/v1/geo/locations' => {'ok': true},
        '/v1/geo/nearby' => {
            'users': [
              {
                'user_id': 'u-2',
                'lat': 1.0,
                'lng': 2.0,
                'distance_m': 3.0,
                'safety_score': 1500.0,
                'safety_vote_count': 0,
              }
            ]
          },
        '/v1/geo/safety/votes' => {'safety_score': 1642.8, 'vote_count': 2},
        '/v1/payments/wallet' => {'balance_cents': 5000, 'currency': 'USD'},
        '/v1/payments/deposits' => {
            'transaction_id': 't-1',
            'status': 'settled',
            'balance_cents': 5000,
          },
        _ => null,
      };

      if (payload == null) {
        // Every unrecognised path is a 404 with the real envelope, so the
        // error mapping is exercised by the geofence delete test.
        request.response.statusCode = 404;
        request.response.write(jsonEncode({
          'error': {'code': 'not_found', 'message': 'geofence not found'}
        }));
      } else {
        request.response.write(jsonEncode(payload));
      }
      await request.response.close();
    });
  }
}

void main() {
  late FakeGateway gateway;

  setUp(() async {
    gateway = await FakeGateway.start();
  });

  tearDown(() async {
    await gateway.close();
  });

  AtlasClient client({int maxRetries = 0, String? token}) => AtlasClient(
        baseUrl: gateway.baseUrl,
        projectKey: testKey,
        token: token,
        maxRetries: maxRetries,
        timeout: const Duration(seconds: 5),
      );

  group('credentials', () {
    test('a missing project key throws at construction', () {
      // Not at call time: a missing key is a configuration mistake, and
      // the stack trace should point at the line that has to change.
      expect(
        () => AtlasClient(baseUrl: 'http://127.0.0.1:1', projectKey: ''),
        throwsA(isA<ArgumentError>()),
      );
    });

    test('the project key is sent on every call, including register', () async {
      final c = client();
      await c.auth.register(email: 'a@b.dev', password: 'hunter2!');
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');
      await c.auth.me();
      c.close();

      expect(gateway.requests, hasLength(3));
      for (final r in gateway.requests) {
        expect(r.key, testKey, reason: 'missing key on ${r.path}');
      }
      // Register and login carry no bearer; /me does.
      expect(gateway.requests[0].authorization, isNull);
      expect(gateway.requests[1].authorization, isNull);
      expect(gateway.requests[2].authorization, 'Bearer tok-abc');
    });

    test('login stores the token, logout forgets it', () async {
      final c = client();
      expect(c.token, isNull);
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');
      expect(c.token, 'tok-abc');
      await c.auth.logout();
      expect(c.token, isNull);
      c.close();
    });

    test('a completed reset clears the token', () async {
      // The server just revoked every session; holding the dead token
      // would only produce confusing 403s on the next call.
      final c = client();
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');
      await c.auth.resetPassword(token: 'f' * 64, newPassword: 'a-new-one');
      expect(c.token, isNull);
      c.close();
    });

    test('verifying an email leaves the session alone', () async {
      final c = client();
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');
      await c.auth.verifyEmail(token: 'a' * 64);
      expect(c.token, 'tok-abc');
      c.close();
    });

    test('toString never contains the project key or token', () async {
      // The default would print the key the first time anything logs a
      // client, and it would land in a crash report.
      final c = client();
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');
      expect(c.toString(), isNot(contains(testKey)));
      expect(c.toString(), isNot(contains('tok-abc')));
      expect(c.toString(), contains('authenticated: true'));
      c.close();
    });
  });

  group('identity', () {
    test('no request body carries a user id or project id', () async {
      final c = client();
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');
      await c.geo.castSafetyVote(lat: 1, lng: 2, verdict: Verdict.unsafe);
      await c.geo.updateLocation(lat: 1, lng: 2);
      c.close();

      for (final r in gateway.requests) {
        expect(r.body, isNot(contains('user_id')), reason: r.path);
        expect(r.body, isNot(contains('project_id')), reason: r.path);
      }
    });
  });

  group('transport', () {
    test('a GET is retried and eventually succeeds', () async {
      gateway.failTimes = 2;
      final c = client(maxRetries: 2);
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');

      final wallet = await c.payments.wallet();
      expect(wallet.balanceCents, 5000);
      expect(gateway.attempts, 3, reason: 'two failures, one success');
      c.close();
    });

    test('a 429 with Retry-After delays the retry', () async {
      gateway.rateLimitedTimes = 1;
      final c = client(maxRetries: 1);
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');

      final started = DateTime.now();
      final wallet = await c.payments.wallet();
      final elapsedMs = DateTime.now().difference(started).inMilliseconds;

      expect(wallet.balanceCents, 5000);
      expect(gateway.requests.length, 3, reason: 'login + 429 + success');
      expect(elapsedMs, greaterThanOrEqualTo(900),
          reason: 'retry should honour Retry-After, took ${elapsedMs}ms');
      c.close();
    });

    test('a deposit is retried and reuses its idempotency key', () async {
      // The key is what makes replaying a POST safe on the server side. A
      // fresh key per retry would double-charge.
      gateway.failTimes = 1;
      final c = client(maxRetries: 2);
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');

      final deposit = await c.payments.deposit(amountCents: 5000);
      expect(deposit.balanceCents, 5000);

      final bodies = gateway.requests
          .where((r) => r.path == '/v1/payments/deposits')
          .map((r) => r.body)
          .toList();
      expect(bodies, hasLength(2));
      expect(bodies[0], bodies[1], reason: 'the retry must reuse the key');
      c.close();
    });

    test('query parameters are encoded', () async {
      final c = client();
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');
      await c.geo.nearby(lat: 51.5074, lng: -0.1278, radiusM: 500);

      final q =
          gateway.requests.firstWhere((r) => r.path == '/v1/geo/nearby').query;
      expect(q['lat'], '51.5074');
      expect(q['lng'], '-0.1278');
      expect(q['radius_m'], '500.0');
      c.close();
    });
  });

  group('errors', () {
    test('an error envelope becomes a typed code', () async {
      final c = client();
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');

      await expectLater(
        c.geo.deleteGeofence('someone-elses'),
        throwsA(isA<AtlasError>()
            .having((e) => e.code, 'code', AtlasErrorCode.notFound)
            .having((e) => e.isRetryable, 'isRetryable', isFalse)),
      );
      c.close();
    });

    test('an unreachable gateway is a connection error, not an API error',
        () async {
      // The distinction matters: "the service rejected this" and "we could
      // not ask" need different handling, and conflating them produces
      // both spurious alerts and missed outages.
      final c = AtlasClient(
        // A port nothing listens on.
        baseUrl: 'http://127.0.0.1:59987',
        projectKey: testKey,
        maxRetries: 0,
        timeout: const Duration(milliseconds: 500),
      );

      await expectLater(
        c.auth.register(email: 'a@b.dev', password: 'hunter2!'),
        throwsA(isA<AtlasConnectionError>()),
      );
      c.close();
    });

    test('scoring no routes fails before the request', () async {
      final c = client();
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');
      final before = gateway.requests.length;

      expect(() => c.geo.scoreRoute([]), throwsA(isA<ArgumentError>()));
      expect(gateway.requests.length, before,
          reason: 'an obviously invalid request should not cost a round trip');
      c.close();
    });
  });

  group('types', () {
    test('an unverified address is null, not zero', () async {
      // 0 is a real timestamp; a caller doing date maths on it renders
      // 1970.
      final c = client();
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');

      final me = await c.auth.me();
      expect(me.emailVerifiedAt, isNull);
      expect(me.isEmailVerified, isFalse);
      expect(me.email, 'a@b.dev');
      c.close();
    });

    test('a neutral score arrives with its evidence', () async {
      final c = client();
      await c.auth.login(email: 'a@b.dev', password: 'hunter2!');

      final users = await c.geo.nearby(lat: 1, lng: 2, radiusM: 100);
      expect(users.single.safetyScore, 1500.0);
      // Zero voters means the score is a default, not a measurement.
      expect(users.single.safetyVoteCount, 0);
      expect(users.single.hasSafetyData, isFalse);
      c.close();
    });
  });
}
