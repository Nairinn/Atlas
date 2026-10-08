/**
 * Public types.
 *
 * These are camelCase; the wire is snake_case. The mapping is written out
 * per field in `client.ts` rather than done by a generic deep-converter:
 * a converter has to guess at edge cases, breaks the moment one field does
 * not follow the rule, and erases the types that make this SDK worth using.
 */

// --- auth -------------------------------------------------------------------

export interface RegisterParams {
  email: string;
  password: string;
}

export interface RegisterResult {
  userId: string;
}

export interface LoginParams {
  email: string;
  password: string;
  /** Optional position stamped into the token's claims. Send both or neither. */
  lat?: number;
  lng?: number;
}

export interface Session {
  token: string;
  /** Unix epoch seconds. */
  expiresAt: number;
}

export interface Claims {
  userId: string;
  sessionId: string;
  /** (0, 0) when no position was supplied at login — the wire has no nulls here. */
  lastLat: number;
  lastLng: number;
  issuedAt: number;
  expiresAt: number;
  email: string;
  /**
   * Unix seconds, or `null` when the address has never been confirmed.
   *
   * A timestamp rather than a boolean because "when" is the question
   * support conversations actually ask. Gate features on
   * `emailVerifiedAt !== null`.
   */
  emailVerifiedAt: number | null;
  createdAt: number;
}

// --- geo --------------------------------------------------------------------

export interface LocationParams {
  lat: number;
  lng: number;
  /** Unix epoch seconds. Omit to let the platform stamp "now". */
  recordedAt?: number;
}

export interface NearbyParams {
  lat: number;
  lng: number;
  /** Metres. Must be > 0 and <= 50000. */
  radiusM: number;
  /** Free-form; used as a metrics label server-side. */
  role?: string;
  /** 1-100. Omit for the server default of 20. */
  limit?: number;
}

export interface NearbyUser {
  userId: string;
  lat: number;
  lng: number;
  distanceM: number;
  /**
   * Safety score for the place this user is standing, in 1000..2000 with
   * 1500 neutral. Derived from safety votes near that position, smoothed
   * toward neutral in proportion to how little evidence there is.
   *
   * Read it together with `safetyVoteCount`: 1500 from nobody voting and
   * 1500 from a hundred evenly split voters are different facts, and a UI
   * that shows them the same way is claiming knowledge it does not have.
   */
  safetyScore: number;
  /** Distinct voters behind `safetyScore`. Zero means "no data". */
  safetyVoteCount: number;
}

/** One user's judgement about one place. */
export type SafetyVerdict = 'safe' | 'unsafe';

export interface SafetyVoteParams {
  lat: number;
  lng: number;
  verdict: SafetyVerdict;
}

export interface SafetyVoteResult {
  /** The area's score after your vote. */
  safetyScore: number;
  /** Distinct voters behind it, you included. */
  voteCount: number;
}

export interface LatLng {
  lat: number;
  lng: number;
}

export interface RouteCandidate {
  routeId: string;
  /** At least two points. */
  points: LatLng[];
}

export interface ScoredRoute {
  routeId: string;
  score: number;
}

export interface RouteScore {
  bestRouteId: string;
  score: number;
  allScores: ScoredRoute[];
}

export interface CreateGeofenceParams {
  label?: string;
  centerLat: number;
  centerLng: number;
  /** Metres. Must be > 0 and <= 50000. */
  radiusM: number;
}

export interface Geofence {
  id: string;
  userId: string;
  /** Empty string when unlabelled — the wire has no null here. */
  label: string;
  centerLat: number;
  centerLng: number;
  radiusM: number;
  active: boolean;
  createdAt: number;
}

export interface GeofenceCheck {
  triggered: boolean;
  geofenceIds: string[];
}

// --- payments ---------------------------------------------------------------

export interface Wallet {
  balanceCents: number;
  currency: string;
}

export interface DepositParams {
  /** Must be > 0. */
  amountCents: number;
  /**
   * Supply this to make retries safe across process restarts. Omitted, the
   * SDK generates one per call — see `createTransaction` for the caveat.
   */
  idempotencyKey?: string;
}

export interface Deposit {
  transactionId: string;
  /** "settled" for a completed top-up. */
  status: string;
  /** Wallet balance after the deposit. */
  balanceCents: number;
}

export interface CreateTransactionParams {
  toUserId: string;
  /** Must be > 0. */
  amountCents: number;
  /**
   * Supply this to make retries safe across process restarts. When it is
   * omitted the SDK generates one per call, which protects against its own
   * transport-level retries but not against your application retrying
   * after a crash.
   */
  idempotencyKey?: string;
  rideId?: string;
}

export interface Transaction {
  transactionId: string;
  /**
   * The processor's client_secret under the client-side-confirmation
   * flow: present it to the processor's client SDK to complete the
   * payment. Absent when the flow does not use one.
   */
  clientSecret?: string;
  /** "pending" | "settled" | "failed" | "refunded" | "cancelled" */
  status: string;
}
