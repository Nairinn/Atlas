// Wire types.
//
// Each parses from the JSON the gateway sends. Where the API's shape would
// be misleading in Dart — a 0 that means "never" — the type says what it
// means instead.

class RegisterResult {
  RegisterResult({required this.userId});

  factory RegisterResult.fromJson(Map<String, dynamic> json) =>
      RegisterResult(userId: json['user_id'] as String);

  final String userId;
}

class Session {
  Session({required this.token, required this.expiresAt});

  factory Session.fromJson(Map<String, dynamic> json) => Session(
        token: json['token'] as String,
        expiresAt: json['expires_at'] as int,
      );

  final String token;

  /// Unix seconds.
  final int expiresAt;
}

class Me {
  Me({
    required this.userId,
    required this.sessionId,
    required this.lastLat,
    required this.lastLng,
    required this.issuedAt,
    required this.expiresAt,
    required this.email,
    required this.emailVerifiedAt,
    required this.createdAt,
  });

  factory Me.fromJson(Map<String, dynamic> json) => Me(
        userId: json['user_id'] as String,
        sessionId: json['session_id'] as String,
        lastLat: (json['last_lat'] as num).toDouble(),
        lastLng: (json['last_lng'] as num).toDouble(),
        issuedAt: json['issued_at'] as int,
        expiresAt: json['expires_at'] as int,
        email: json['email'] as String? ?? '',
        // `as int?` rather than a default of 0: an older gateway omits the
        // field entirely, and 0 is a real timestamp that renders as 1970.
        emailVerifiedAt: json['email_verified_at'] as int?,
        createdAt: json['created_at'] as int? ?? 0,
      );

  final String userId;
  final String sessionId;

  /// `(0, 0)` when no position was supplied at login — the wire has no
  /// nulls here.
  final double lastLat;
  final double lastLng;
  final int issuedAt;
  final int expiresAt;
  final String email;

  /// Unix seconds, or null when the address has never been confirmed.
  ///
  /// Gate features on `emailVerifiedAt != null`. A timestamp rather than a
  /// bool because "when" is the question support conversations ask, and a
  /// bool cannot be widened into one later without having lost the answer.
  final int? emailVerifiedAt;
  final int createdAt;

  bool get isEmailVerified => emailVerifiedAt != null;
}

class NearbyUser {
  NearbyUser({
    required this.userId,
    required this.lat,
    required this.lng,
    required this.distanceM,
    required this.safetyScore,
    required this.safetyVoteCount,
  });

  factory NearbyUser.fromJson(Map<String, dynamic> json) => NearbyUser(
        userId: json['user_id'] as String,
        lat: (json['lat'] as num).toDouble(),
        lng: (json['lng'] as num).toDouble(),
        distanceM: (json['distance_m'] as num).toDouble(),
        safetyScore: (json['safety_score'] as num).toDouble(),
        safetyVoteCount: json['safety_vote_count'] as int? ?? 0,
      );

  final String userId;
  final double lat;
  final double lng;
  final double distanceM;

  /// Safety score for the place this user is standing: 1000..2000,
  /// neutral 1500.
  ///
  /// Read with [safetyVoteCount]. 1500 from nobody voting and 1500 from a
  /// hundred evenly split voters are different facts, and a UI that
  /// renders them identically is claiming knowledge it does not have.
  final double safetyScore;

  /// Distinct voters behind [safetyScore]. Zero means "no data".
  final int safetyVoteCount;

  bool get hasSafetyData => safetyVoteCount > 0;
}

class Geofence {
  Geofence({
    required this.id,
    required this.userId,
    required this.label,
    required this.centerLat,
    required this.centerLng,
    required this.radiusM,
    required this.active,
    required this.createdAt,
  });

  factory Geofence.fromJson(Map<String, dynamic> json) => Geofence(
        id: json['id'] as String,
        userId: json['user_id'] as String,
        label: json['label'] as String? ?? '',
        centerLat: (json['center_lat'] as num).toDouble(),
        centerLng: (json['center_lng'] as num).toDouble(),
        radiusM: (json['radius_m'] as num).toDouble(),
        active: json['active'] as bool,
        createdAt: json['created_at'] as int,
      );

  final String id;
  final String userId;

  /// Empty string when unlabelled.
  final String label;
  final double centerLat;
  final double centerLng;
  final double radiusM;
  final bool active;
  final int createdAt;
}

class GeofenceCheck {
  GeofenceCheck({required this.triggered, required this.geofenceIds});

  factory GeofenceCheck.fromJson(Map<String, dynamic> json) => GeofenceCheck(
        triggered: json['triggered'] as bool,
        geofenceIds: (json['geofence_ids'] as List<dynamic>? ?? const [])
            .map((e) => e as String)
            .toList(),
      );

  final bool triggered;
  final List<String> geofenceIds;
}

class LatLng {
  const LatLng(this.lat, this.lng);

  final double lat;
  final double lng;

  Map<String, dynamic> toJson() => {'lat': lat, 'lng': lng};
}

class RouteCandidate {
  const RouteCandidate({required this.routeId, required this.points});

  final String routeId;
  final List<LatLng> points;

  Map<String, dynamic> toJson() => {
        'route_id': routeId,
        'points': points.map((p) => p.toJson()).toList(),
      };
}

class ScoredRoute {
  ScoredRoute({
    required this.routeId,
    required this.score,
    required this.voteCount,
  });

  factory ScoredRoute.fromJson(Map<String, dynamic> json) => ScoredRoute(
        routeId: json['route_id'] as String,
        score: (json['score'] as num).toDouble(),
        voteCount: json['vote_count'] as int? ?? 0,
      );

  final String routeId;
  final double score;
  final int voteCount;
}

class RouteScore {
  RouteScore({
    required this.bestRouteId,
    required this.score,
    required this.allScores,
  });

  factory RouteScore.fromJson(Map<String, dynamic> json) => RouteScore(
        bestRouteId: json['best_route_id'] as String,
        score: (json['score'] as num).toDouble(),
        allScores: (json['all_scores'] as List<dynamic>? ?? const [])
            .map((e) => ScoredRoute.fromJson(e as Map<String, dynamic>))
            .toList(),
      );

  final String bestRouteId;
  final double score;
  final List<ScoredRoute> allScores;
}

/// One user's judgement about one place.
enum Verdict {
  safe('safe'),
  unsafe('unsafe');

  const Verdict(this.wire);

  final String wire;
}

class SafetyVote {
  SafetyVote({required this.safetyScore, required this.voteCount});

  factory SafetyVote.fromJson(Map<String, dynamic> json) => SafetyVote(
        safetyScore: (json['safety_score'] as num).toDouble(),
        voteCount: json['vote_count'] as int? ?? 0,
      );

  final double safetyScore;
  final int voteCount;
}

class Wallet {
  Wallet({required this.balanceCents, required this.currency});

  factory Wallet.fromJson(Map<String, dynamic> json) => Wallet(
        balanceCents: json['balance_cents'] as int,
        currency: json['currency'] as String,
      );

  final int balanceCents;
  final String currency;
}

class Deposit {
  Deposit({
    required this.transactionId,
    required this.status,
    required this.balanceCents,
  });

  factory Deposit.fromJson(Map<String, dynamic> json) => Deposit(
        transactionId: json['transaction_id'] as String,
        status: json['status'] as String,
        balanceCents: json['balance_cents'] as int,
      );

  final String transactionId;
  final String status;
  final int balanceCents;
}

class Transaction {
  Transaction(
      {required this.transactionId, required this.status, this.clientSecret});

  factory Transaction.fromJson(Map<String, dynamic> json) => Transaction(
        transactionId: json['transaction_id'] as String,
        status: json['status'] as String,
        // The processor's client_secret under the client-side-confirmation
        // flow; null when the flow does not use one.
        clientSecret: (json['client_secret'] as String?)?.isEmpty == true
            ? null
            : json['client_secret'] as String?,
      );

  final String transactionId;
  final String status;
  final String? clientSecret;
}

class OnboardingUrl {
  OnboardingUrl({required this.onboardingUrl});

  factory OnboardingUrl.fromJson(Map<String, dynamic> json) =>
      OnboardingUrl(onboardingUrl: json['onboarding_url'] as String);

  final String onboardingUrl;
}

class PayoutAccountStatus {
  PayoutAccountStatus({required this.exists, required this.payoutsEnabled});

  factory PayoutAccountStatus.fromJson(Map<String, dynamic> json) =>
      PayoutAccountStatus(
        exists: json['exists'] as bool,
        payoutsEnabled: json['payouts_enabled'] as bool,
      );

  final bool exists;
  final bool payoutsEnabled;
}
