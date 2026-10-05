/// Dart client for the Atlas gateway.
///
/// ```dart
/// final atlas = AtlasClient(
///   baseUrl: 'https://api.atlas.dev',
///   projectKey: Platform.environment['ATLAS_KEY']!,
/// );
///
/// await atlas.auth.login(email: 'rider@example.com', password: 'hunter2!');
/// final users = await atlas.geo.nearby(lat: 51.5074, lng: -0.1278, radiusM: 500);
/// ```
///
/// # Two credentials
///
/// Every call carries both, and they answer different questions. The
/// `projectKey` says which application is calling — it is yours, it stays
/// on your server, and it never changes between users. The bearer token
/// says which of your users is calling; [AuthApi.login] obtains it and
/// stores it on the client. Neither substitutes for the other.
///
/// **The project key is a server-side secret.** Anyone holding it can act
/// on your whole project, so it must not ship inside a Flutter app —
/// which is the obvious thing to do with a Dart SDK and the reason this
/// warning is here rather than buried in a README. Put this client behind
/// your own API.
///
/// # Identity
///
/// No method takes a user id or a project id. The gateway derives both
/// from the credentials above, and its request bodies have no fields for
/// them — that absence is what stops one caller acting as another.
library;

import 'dart:io';
import 'dart:math';

import 'src/errors.dart';
import 'src/http.dart';
import 'src/types.dart';

export 'src/errors.dart';
export 'src/types.dart';

/// Client for the Atlas gateway.
class AtlasClient {
  AtlasClient({
    required String baseUrl,
    required String projectKey,
    String? token,
    Duration timeout = const Duration(seconds: 10),
    int maxRetries = 2,
    HttpClient? httpClient,
  }) {
    if (projectKey.isEmpty) {
      // Thrown here rather than letting every call come back 401: a
      // missing key is a configuration mistake, and the stack trace should
      // point at the line that has to change.
      throw ArgumentError.value(
        projectKey,
        'projectKey',
        'required (atl_live_… / atl_test_… / atl_dev_…)',
      );
    }
    if (baseUrl.isEmpty) {
      throw ArgumentError.value(baseUrl, 'baseUrl', 'required');
    }
    _http = Http(
      baseUrl: baseUrl,
      projectKey: projectKey,
      timeout: timeout,
      maxRetries: maxRetries,
      client: httpClient,
    );
    _http.token = token;

    auth = AuthApi._(_http);
    geo = GeoApi._(_http);
    payments = PaymentsApi._(_http);
  }

  late final Http _http;

  late final AuthApi auth;
  late final GeoApi geo;
  late final PaymentsApi payments;

  /// The current bearer token, if any.
  String? get token => _http.token;

  /// Set or clear the token. `login` and `logout` do this for you.
  set token(String? value) => _http.token = value;

  /// Release the underlying connections. Call when you are done with the
  /// client; a long-lived app can simply keep one.
  void close() => _http.close();

  /// Deliberately says nothing about credentials: the default would print
  /// the project key the first time anything logs a client.
  @override
  String toString() => 'AtlasClient(authenticated: ${_http.token != null})';
}

class AuthApi {
  AuthApi._(this._http);

  final Http _http;

  /// Create a user in the calling project.
  ///
  /// The same address in two projects is two different people, so this
  /// conflicts only within one project.
  Future<RegisterResult> register({
    required String email,
    required String password,
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/auth/register',
      authenticated: false,
      body: {'email': email, 'password': password},
    );
    return RegisterResult.fromJson(json as Map<String, dynamic>);
  }

  /// Exchange credentials for a token, which is stored on the client.
  Future<Session> login({
    required String email,
    required String password,
    double? lat,
    double? lng,
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/auth/login',
      authenticated: false,
      body: {
        'email': email,
        'password': password,
        if (lat != null && lng != null) 'lat': lat,
        if (lat != null && lng != null) 'lng': lng,
      },
    );
    final session = Session.fromJson(json as Map<String, dynamic>);
    _http.token = session.token;
    return session;
  }

  /// Revoke the current session and forget the token.
  Future<void> logout() async {
    try {
      await _http.send(method: 'POST', path: '/v1/auth/logout');
    } finally {
      // Cleared regardless of the outcome: the token is either revoked or
      // was already invalid, and keeping it helps nobody.
      _http.token = null;
    }
  }

  /// The current session and the caller's profile.
  Future<Me> me() async {
    final json = await _http.send(method: 'GET', path: '/v1/auth/me');
    return Me.fromJson(json as Map<String, dynamic>);
  }

  /// Ask Atlas to mail this address a password reset link.
  ///
  /// Completes the same way whether or not the address has an account —
  /// the server deliberately does not say, since an endpoint that did
  /// would let anyone test a list of addresses for which have accounts.
  /// Do not build a UI that claims the address was found.
  Future<void> requestPasswordReset({required String email}) async {
    await _http.send(
      method: 'POST',
      path: '/v1/auth/password-reset',
      authenticated: false,
      body: {'email': email},
    );
  }

  /// Redeem a reset token and set a new password.
  ///
  /// Needs no session: whoever holds the emailed token is, for this one
  /// call, the account's owner. Succeeding revokes every session the user
  /// had — including this client's, so the stored token is cleared and
  /// you must log in again.
  Future<String> resetPassword({
    required String token,
    required String newPassword,
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/auth/password-reset/confirm',
      authenticated: false,
      body: {'token': token, 'new_password': newPassword},
    );
    _http.token = null;
    return (json as Map<String, dynamic>)['user_id'] as String;
  }

  /// Ask Atlas to mail this address a verification link. Also silent
  /// about whether the address exists.
  Future<void> requestEmailVerification({required String email}) async {
    await _http.send(
      method: 'POST',
      path: '/v1/auth/email/verify',
      authenticated: false,
      body: {'email': email},
    );
  }

  /// Redeem a verification token.
  ///
  /// Unlike a reset this leaves sessions alone: confirming an address is
  /// not evidence that anything leaked.
  Future<String> verifyEmail({required String token}) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/auth/email/verify/confirm',
      authenticated: false,
      body: {'token': token},
    );
    return (json as Map<String, dynamic>)['user_id'] as String;
  }
}

class GeoApi {
  GeoApi._(this._http);

  final Http _http;

  /// Record the caller's position.
  Future<bool> updateLocation({
    required double lat,
    required double lng,
    int? recordedAt,
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/geo/locations',
      body: {
        'lat': lat,
        'lng': lng,
        if (recordedAt != null) 'recorded_at': recordedAt,
      },
    );
    return (json as Map<String, dynamic>)['ok'] as bool? ?? false;
  }

  /// Users within [radiusM] metres, nearest first.
  ///
  /// Scoped to the calling project: it never returns another customer's
  /// users, even standing on the same coordinates.
  Future<List<NearbyUser>> nearby({
    required double lat,
    required double lng,
    required double radiusM,
    String? role,
    int? limit,
  }) async {
    final json = await _http.send(
      method: 'GET',
      path: '/v1/geo/nearby',
      query: {
        'lat': '$lat',
        'lng': '$lng',
        'radius_m': '$radiusM',
        if (role != null) 'role': role,
        if (limit != null) 'limit': '$limit',
      },
    );
    final users = (json as Map<String, dynamic>)['users'] as List<dynamic>;
    return users
        .map((e) => NearbyUser.fromJson(e as Map<String, dynamic>))
        .toList();
  }

  /// Rank route candidates by the safety votes along them.
  Future<RouteScore> scoreRoute(List<RouteCandidate> candidates) async {
    if (candidates.isEmpty) {
      throw ArgumentError.value(
        candidates,
        'candidates',
        'at least one route candidate is required',
      );
    }
    final json = await _http.send(
      method: 'POST',
      path: '/v1/geo/routes/score',
      body: {'candidates': candidates.map((c) => c.toJson()).toList()},
    );
    return RouteScore.fromJson(json as Map<String, dynamic>);
  }

  Future<Geofence> createGeofence({
    required double centerLat,
    required double centerLng,
    required double radiusM,
    String label = '',
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/geo/geofences',
      body: {
        'label': label,
        'center_lat': centerLat,
        'center_lng': centerLng,
        'radius_m': radiusM,
      },
    );
    return Geofence.fromJson(json as Map<String, dynamic>);
  }

  Future<List<Geofence>> listGeofences({bool activeOnly = false}) async {
    final json = await _http.send(
      method: 'GET',
      path: '/v1/geo/geofences',
      query: {'active_only': '$activeOnly'},
    );
    final fences = (json as Map<String, dynamic>)['geofences'] as List<dynamic>;
    return fences
        .map((e) => Geofence.fromJson(e as Map<String, dynamic>))
        .toList();
  }

  /// Deactivate one of the caller's geofences.
  ///
  /// A fence belonging to someone else is a [AtlasErrorCode.notFound], the
  /// same answer as an id that never existed.
  Future<bool> deleteGeofence(String id) async {
    final json = await _http.send(
      method: 'DELETE',
      path: '/v1/geo/geofences/$id',
    );
    return (json as Map<String, dynamic>)['deleted'] as bool? ?? false;
  }

  Future<GeofenceCheck> checkGeofences({
    required double lat,
    required double lng,
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/geo/geofences/check',
      body: {'lat': lat, 'lng': lng},
    );
    return GeofenceCheck.fromJson(json as Map<String, dynamic>);
  }

  /// Record the caller's judgement about a place.
  ///
  /// Voting again in the same area replaces your previous verdict rather
  /// than adding to it: one user is one voter however often they vote.
  Future<SafetyVote> castSafetyVote({
    required double lat,
    required double lng,
    required Verdict verdict,
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/geo/safety/votes',
      body: {'lat': lat, 'lng': lng, 'verdict': verdict.wire},
    );
    return SafetyVote.fromJson(json as Map<String, dynamic>);
  }
}

class PaymentsApi {
  PaymentsApi._(this._http);

  final Http _http;
  final Random _random = Random.secure();

  Future<Wallet> wallet() async {
    final json = await _http.send(method: 'GET', path: '/v1/payments/wallet');
    return Wallet.fromJson(json as Map<String, dynamic>);
  }

  /// Add funds to the caller's wallet.
  ///
  /// The idempotency key is generated when omitted, and it is what makes
  /// this POST safe for the transport to retry.
  Future<Deposit> deposit({
    required int amountCents,
    String? idempotencyKey,
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/payments/deposits',
      replayable: true,
      body: {
        'amount_cents': amountCents,
        'idempotency_key': idempotencyKey ?? _newKey(),
      },
    );
    return Deposit.fromJson(json as Map<String, dynamic>);
  }

  /// Move funds from the caller to another user.
  ///
  /// Creates a PENDING transaction. The money moves later, when your
  /// application publishes the ride lifecycle event that settles it —
  /// there is deliberately no settle method, because the gateway cannot
  /// verify the caller owns a transaction.
  Future<Transaction> createTransaction({
    required String toUserId,
    required int amountCents,
    String? rideId,
    String? idempotencyKey,
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/payments/transactions',
      replayable: true,
      body: {
        'to_user_id': toUserId,
        'amount_cents': amountCents,
        'idempotency_key': idempotencyKey ?? _newKey(),
        if (rideId != null) 'ride_id': rideId,
      },
    );
    return Transaction.fromJson(json as Map<String, dynamic>);
  }

  /// Start payout onboarding for the CALLER's own account.
  ///
  /// The returned URL is single-use and short-lived: redirect the user
  /// immediately. The account being onboarded is the token subject's,
  /// so a caller can only ever set up payouts for themselves.
  Future<OnboardingUrl> startPayoutOnboarding({
    required String returnUrl,
  }) async {
    final json = await _http.send(
      method: 'POST',
      path: '/v1/payments/connected-accounts',
      body: {'return_url': returnUrl},
    );
    return OnboardingUrl.fromJson(json as Map<String, dynamic>);
  }

  /// The caller's own payout-account status. `payoutsEnabled` is what
  /// gates whether a transaction naming this user as payee can succeed.
  Future<PayoutAccountStatus> payoutAccount() async {
    final json = await _http.send(
      method: 'GET',
      path: '/v1/payments/connected-accounts',
    );
    return PayoutAccountStatus.fromJson(json as Map<String, dynamic>);
  }

  /// A random idempotency key.
  ///
  /// `Random.secure()` rather than the default generator: a predictable
  /// key from one client could collide with another's, and on this
  /// endpoint a collision means one caller's payment is reported to
  /// another as an idempotent replay.
  String _newKey() {
    final bytes = List<int>.generate(16, (_) => _random.nextInt(256));
    return bytes.map((b) => b.toRadixString(16).padLeft(2, '0')).join();
  }
}
