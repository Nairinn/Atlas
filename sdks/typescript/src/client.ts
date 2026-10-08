import { Http } from './http.js';
import type {
  Claims,
  Deposit,
  DepositParams,
  CreateGeofenceParams,
  CreateTransactionParams,
  Geofence,
  GeofenceCheck,
  LocationParams,
  LoginParams,
  NearbyParams,
  NearbyUser,
  RegisterParams,
  RegisterResult,
  RouteCandidate,
  RouteScore,
  SafetyVoteParams,
  SafetyVoteResult,
  Session,
  Transaction,
  Wallet,
} from './types.js';

export interface AtlasClientOptions {
  /** Gateway origin, e.g. https://api.atlas.dev. The /v1 prefix is added. */
  baseUrl: string;
  /**
   * Your Atlas project key, e.g. `atl_live_…`. Identifies which project
   * the request belongs to and is sent on every call.
   *
   * This is a server-side secret. It is not a user credential and must
   * not be shipped to a browser or a mobile app — anyone holding it can
   * act on your whole project. Put it in an environment variable and keep
   * this client on your backend.
   */
  projectKey: string;
  /** Existing bearer token. `login()` sets this for you. */
  token?: string;
  /** Per-request timeout. Default 10s, matching the gateway's own upstream deadline. */
  timeoutMs?: number;
  /** Retries for safe requests only. Default 2. */
  maxRetries?: number;
  fetch?: typeof globalThis.fetch;
  headers?: Record<string, string>;
}

/**
 * Client for the Atlas gateway.
 *
 * ```ts
 * const atlas = new AtlasClient({
 *   baseUrl: 'https://api.atlas.dev',
 *   projectKey: process.env.ATLAS_KEY!,
 * });
 * await atlas.auth.login({ email, password });   // token stored on the client
 * const { users } = await atlas.geo.nearby({ lat, lng, radiusM: 500 });
 * ```
 *
 * # Identity
 *
 * Two credentials, and they answer different questions. The `projectKey`
 * says which application is calling — it is yours, it stays on your
 * server, and it never changes between users. The bearer token says which
 * of your users is calling, and `login()` obtains it. Neither substitutes
 * for the other.
 *
 * No method takes a user id. The gateway derives it from the token on
 * every authenticated call, and its request bodies have no `user_id`
 * field at all — that absence is what stops one caller acting as another.
 * An SDK that accepted a user id would be implying a capability the API
 * does not have.
 */
export class AtlasClient {
  private readonly http: Http;
  private token: string | undefined;

  readonly auth: AuthApi;
  readonly geo: GeoApi;
  readonly payments: PaymentsApi;

  constructor(opts: AtlasClientOptions) {
    if (!opts.projectKey) {
      // Fail here rather than letting every call come back 401. A missing
      // key is a configuration mistake, and the stack trace from the
      // constructor points at the line that has to change.
      throw new Error('AtlasClient requires a projectKey (atl_live_… / atl_test_… / atl_dev_…)');
    }
    this.http = new Http({
      baseUrl: opts.baseUrl,
      timeoutMs: opts.timeoutMs ?? 10_000,
      maxRetries: opts.maxRetries ?? 2,
      ...(opts.fetch ? { fetch: opts.fetch } : {}),
      headers: {
        ...(opts.headers ?? {}),
        // Last, so it wins. A caller-supplied `headers` map that happened
        // to contain X-Atlas-Key would otherwise silently send every
        // request as a different project — the kind of mistake that only
        // surfaces once it is in production.
        //
        // Sent on every request, including register and login: creating a
        // user means creating them in a project.
        'X-Atlas-Key': opts.projectKey,
      },
    });
    this.token = opts.token;

    this.auth = new AuthApi(this.http, this);
    this.geo = new GeoApi(this.http, this);
    this.payments = new PaymentsApi(this.http, this);
  }

  /** The current bearer token, if any. */
  getToken(): string | undefined {
    return this.token;
  }

  /** Set or clear the token. `login()` calls this; `logout()` clears it. */
  setToken(token: string | undefined): void {
    this.token = token;
  }
}

/** Internal: namespaces read the token through this at call time, never cache it. */
interface TokenHolder {
  getToken(): string | undefined;
  setToken(token: string | undefined): void;
}

// --- auth -------------------------------------------------------------------

class AuthApi {
  constructor(
    private readonly http: Http,
    private readonly holder: TokenHolder,
  ) {}

  /** Create an account. Does not log in — call `login` next. */
  async register(params: RegisterParams): Promise<RegisterResult> {
    const res = await this.http.request<{ user_id: string }>({
      method: 'POST',
      path: '/v1/auth/register',
      body: { email: params.email, password: params.password },
    });
    return { userId: res.user_id };
  }

  /**
   * Exchange credentials for a token, and store it on the client so
   * subsequent calls are authenticated.
   */
  async login(params: LoginParams): Promise<Session> {
    // The gateway rejects one coordinate without the other, so send both
    // or neither rather than letting a half-filled form 400.
    const hasPosition = params.lat !== undefined && params.lng !== undefined;
    const res = await this.http.request<{ token: string; expires_at: number }>({
      method: 'POST',
      path: '/v1/auth/login',
      body: {
        email: params.email,
        password: params.password,
        ...(hasPosition ? { lat: params.lat, lng: params.lng } : {}),
      },
    });
    this.holder.setToken(res.token);
    return { token: res.token, expiresAt: res.expires_at };
  }

  /** Revoke the current token and clear it from the client. */
  async logout(): Promise<{ success: boolean }> {
    const res = await this.http.request<{ success: boolean }>({
      method: 'POST',
      path: '/v1/auth/logout',
      token: this.holder.getToken(),
    });
    // Cleared regardless of the reported result: the token is either
    // revoked or was already invalid, and keeping it helps nobody.
    this.holder.setToken(undefined);
    return res;
  }

  /**
   * Ask Atlas to mail this address a password reset link.
   *
   * Resolves the same way whether or not the address has an account. That
   * is deliberate on the server side — an endpoint that distinguished
   * them would let anyone test a list of addresses for which have
   * accounts — so do not build a UI that claims the address was found.
   */
  async requestPasswordReset(params: { email: string }): Promise<void> {
    await this.http.request<{ accepted: boolean }>({
      method: 'POST',
      path: '/v1/auth/password-reset',
      body: { email: params.email },
    });
  }

  /**
   * Redeem a reset token and set a new password.
   *
   * Needs no logged-in session: whoever holds the emailed token is, for
   * this one call, the account's owner. Succeeding revokes every session
   * the user had, including this client's — call `login` afterwards.
   */
  async resetPassword(params: { token: string; newPassword: string }): Promise<{ userId: string }> {
    const res = await this.http.request<{ user_id: string }>({
      method: 'POST',
      path: '/v1/auth/password-reset/confirm',
      body: { token: params.token, new_password: params.newPassword },
    });
    // The server just revoked every session; holding the dead token would
    // only produce confusing 403s on the next call.
    this.holder.setToken(undefined);
    return { userId: res.user_id };
  }

  /** Ask Atlas to mail this address a verification link. Also silent. */
  async requestEmailVerification(params: { email: string }): Promise<void> {
    await this.http.request<{ accepted: boolean }>({
      method: 'POST',
      path: '/v1/auth/email/verify',
      body: { email: params.email },
    });
  }

  /**
   * Redeem a verification token.
   *
   * Unlike a reset this leaves sessions alone — confirming an address is
   * not evidence that anything leaked.
   */
  async verifyEmail(params: { token: string }): Promise<{ userId: string }> {
    const res = await this.http.request<{ user_id: string }>({
      method: 'POST',
      path: '/v1/auth/email/verify/confirm',
      body: { token: params.token },
    });
    return { userId: res.user_id };
  }

  /**
   * The current session and the caller's profile.
   *
   * Includes `emailVerifiedAt`, so this is how an application gates
   * features on a confirmed address.
   */
  async me(): Promise<Claims> {
    const res = await this.http.request<{
      user_id: string;
      session_id: string;
      last_lat: number;
      last_lng: number;
      issued_at: number;
      expires_at: number;
      email: string;
      email_verified_at: number | null;
      created_at: number;
    }>({
      method: 'GET',
      path: '/v1/auth/me',
      token: this.holder.getToken(),
    });
    return {
      userId: res.user_id,
      sessionId: res.session_id,
      lastLat: res.last_lat,
      lastLng: res.last_lng,
      issuedAt: res.issued_at,
      expiresAt: res.expires_at,
      email: res.email,
      // `?? null` rather than trusting the field to be present: an older
      // gateway omits it entirely, and `undefined` would defeat the
      // `!== null` check the docs tell people to write.
      emailVerifiedAt: res.email_verified_at ?? null,
      createdAt: res.created_at,
    };
  }
}

// --- geo --------------------------------------------------------------------

class GeoApi {
  constructor(
    private readonly http: Http,
    private readonly holder: TokenHolder,
  ) {}

  /** Record the caller's position. */
  async updateLocation(params: LocationParams): Promise<{ ok: boolean }> {
    return this.http.request<{ ok: boolean }>({
      method: 'POST',
      path: '/v1/geo/locations',
      token: this.holder.getToken(),
      body: {
        lat: params.lat,
        lng: params.lng,
        ...(params.recordedAt !== undefined
          ? { recorded_at: params.recordedAt }
          : {}),
      },
    });
  }

  /** Find other users within `radiusM` metres, nearest first. */
  async nearby(params: NearbyParams): Promise<{ users: NearbyUser[] }> {
    const res = await this.http.request<{
      users: Array<{
        user_id: string;
        lat: number;
        lng: number;
        distance_m: number;
        safety_score: number;
        safety_vote_count: number;
      }>;
    }>({
      method: 'GET',
      path: '/v1/geo/nearby',
      token: this.holder.getToken(),
      query: {
        lat: params.lat,
        lng: params.lng,
        radius_m: params.radiusM,
        role: params.role,
        limit: params.limit,
      },
    });
    return {
      users: res.users.map((u) => ({
        userId: u.user_id,
        lat: u.lat,
        lng: u.lng,
        distanceM: u.distance_m,
        safetyScore: u.safety_score,
        safetyVoteCount: u.safety_vote_count,
      })),
    };
  }

  /**
   * Record the caller's judgement about a place.
   *
   * The vote is attributed to the logged-in user, so this needs a token.
   * Voting again in the same area replaces your previous verdict rather
   * than adding to it — opinions change, and one user is one voter however
   * often they vote.
   */
  async castSafetyVote(params: SafetyVoteParams): Promise<SafetyVoteResult> {
    const res = await this.http.request<{
      safety_score: number;
      vote_count: number;
    }>({
      method: 'POST',
      path: '/v1/geo/safety/votes',
      token: this.holder.getToken(),
      body: { lat: params.lat, lng: params.lng, verdict: params.verdict },
    });
    return { safetyScore: res.safety_score, voteCount: res.vote_count };
  }

  /** Score route candidates against nearby safety votes. Highest wins. */
  async scoreRoute(candidates: RouteCandidate[]): Promise<RouteScore> {
    const res = await this.http.request<{
      best_route_id: string;
      score: number;
      all_scores: Array<{ route_id: string; score: number }>;
    }>({
      method: 'POST',
      path: '/v1/geo/routes/score',
      token: this.holder.getToken(),
      body: {
        candidates: candidates.map((c) => ({
          route_id: c.routeId,
          points: c.points.map((p) => ({ lat: p.lat, lng: p.lng })),
        })),
      },
    });
    return {
      bestRouteId: res.best_route_id,
      score: res.score,
      allScores: res.all_scores.map((s) => ({
        routeId: s.route_id,
        score: s.score,
      })),
    };
  }

  async createGeofence(params: CreateGeofenceParams): Promise<Geofence> {
    const res = await this.http.request<WireGeofence>({
      method: 'POST',
      path: '/v1/geo/geofences',
      token: this.holder.getToken(),
      body: {
        ...(params.label !== undefined ? { label: params.label } : {}),
        center_lat: params.centerLat,
        center_lng: params.centerLng,
        radius_m: params.radiusM,
      },
    });
    return toGeofence(res);
  }

  async listGeofences(opts: { activeOnly?: boolean } = {}): Promise<Geofence[]> {
    const res = await this.http.request<{ geofences: WireGeofence[] }>({
      method: 'GET',
      path: '/v1/geo/geofences',
      token: this.holder.getToken(),
      query: { active_only: opts.activeOnly },
    });
    return res.geofences.map(toGeofence);
  }

  /**
   * Deactivate a geofence. This is a soft delete: the fence stops matching
   * but stays visible to `listGeofences({ activeOnly: false })`.
   */
  async deleteGeofence(geofenceId: string): Promise<{ deleted: boolean }> {
    return this.http.request<{ deleted: boolean }>({
      method: 'DELETE',
      path: `/v1/geo/geofences/${encodeURIComponent(geofenceId)}`,
      token: this.holder.getToken(),
    });
  }

  /** Which of the caller's geofences contain this point right now. */
  async checkGeofences(point: {
    lat: number;
    lng: number;
  }): Promise<GeofenceCheck> {
    const res = await this.http.request<{
      triggered: boolean;
      geofence_ids: string[];
    }>({
      method: 'POST',
      path: '/v1/geo/geofences/check',
      token: this.holder.getToken(),
      body: { lat: point.lat, lng: point.lng },
    });
    return { triggered: res.triggered, geofenceIds: res.geofence_ids };
  }
}

interface WireGeofence {
  id: string;
  user_id: string;
  label: string;
  center_lat: number;
  center_lng: number;
  radius_m: number;
  active: boolean;
  created_at: number;
}

function toGeofence(g: WireGeofence): Geofence {
  return {
    id: g.id,
    userId: g.user_id,
    label: g.label,
    centerLat: g.center_lat,
    centerLng: g.center_lng,
    radiusM: g.radius_m,
    active: g.active,
    createdAt: g.created_at,
  };
}

// --- payments ---------------------------------------------------------------

class PaymentsApi {
  constructor(
    private readonly http: Http,
    private readonly holder: TokenHolder,
  ) {}

  /**
   * Add funds to the caller's own wallet from an external payment method.
   *
   * This is the only way money enters the platform — wallets start empty
   * and a transfer cannot move funds that are not there. Like
   * `createTransaction` it always sends an idempotency key, which is what
   * makes it safe for the transport to retry.
   *
   * There is no `toUserId`: the wallet credited is the token's subject, so
   * a caller can only ever top up themselves.
   */
  async deposit(params: DepositParams): Promise<Deposit> {
    const idempotencyKey = params.idempotencyKey ?? randomKey();
    const res = await this.http.request<{
      transaction_id: string;
      status: string;
      balance_cents: number;
    }>({
      method: 'POST',
      path: '/v1/payments/deposits',
      token: this.holder.getToken(),
      headers: { 'Idempotency-Key': idempotencyKey },
      body: { amount_cents: params.amountCents },
    });
    return {
      transactionId: res.transaction_id,
      status: res.status,
      balanceCents: res.balance_cents,
    };
  }

  /** The caller's own wallet. There is no way to read anyone else's. */
  async wallet(): Promise<Wallet> {
    const res = await this.http.request<{
      balance_cents: number;
      currency: string;
    }>({
      method: 'GET',
      path: '/v1/payments/wallet',
      token: this.holder.getToken(),
    });
    return { balanceCents: res.balance_cents, currency: res.currency };
  }

  /**
   * Move money from the caller's wallet to another user.
   *
   * An idempotency key is always sent — supplied by you, or generated
   * here. That is what lets the transport retry this call at all; without
   * a key it would be the one request in the SDK that can never be
   * repeated safely. A generated key covers retries within this call, but
   * only a key you persist covers your own process crashing and retrying.
   *
   * There is deliberately no `settle` or `refund` here: those are not
   * exposed through the gateway, because it has no way to check that the
   * caller owns the transaction. Settlement is driven server-side by ride
   * lifecycle events.
   */
  async createTransaction(
    params: CreateTransactionParams,
  ): Promise<Transaction> {
    const idempotencyKey = params.idempotencyKey ?? randomKey();
    const res = await this.http.request<{
      transaction_id: string;
      status: string;
      client_secret: string;
    }>({
      method: 'POST',
      path: '/v1/payments/transactions',
      token: this.holder.getToken(),
      // Sent as a header rather than in the body: it is the conventional
      // location, and the gateway prefers the header when both are present.
      headers: { 'Idempotency-Key': idempotencyKey },
      body: {
        to_user_id: params.toUserId,
        amount_cents: params.amountCents,
        ...(params.rideId !== undefined ? { ride_id: params.rideId } : {}),
      },
    });
    return {
      transactionId: res.transaction_id,
      status: res.status,
      ...(res.client_secret ? { clientSecret: res.client_secret } : {}),
    };
  }

  /**
   * Start payout onboarding for the CALLER's own account.
   *
   * Returns the hosted onboarding URL, which is single-use and
   * short-lived: redirect the user immediately. The account being
   * onboarded is the token subject's, so a caller can only ever set up
   * payouts for themselves.
   */
  async startPayoutOnboarding(params: {
    returnUrl: string;
  }): Promise<{ onboardingUrl: string }> {
    const res = await this.http.request<{ onboarding_url: string }>({
      method: 'POST',
      path: '/v1/payments/connected-accounts',
      token: this.holder.getToken(),
      body: { return_url: params.returnUrl },
    });
    return { onboardingUrl: res.onboarding_url };
  }

  /**
   * The caller's own payout-account status. `payoutsEnabled` is what
   * gates whether a transaction naming this user as payee can succeed.
   */
  async payoutAccount(): Promise<{
    exists: boolean;
    payoutsEnabled: boolean;
  }> {
    const res = await this.http.request<{
      exists: boolean;
      payouts_enabled: boolean;
    }>({
      method: 'GET',
      path: '/v1/payments/connected-accounts',
      token: this.holder.getToken(),
    });
    return { exists: res.exists, payoutsEnabled: res.payouts_enabled };
  }
}

function randomKey(): string {
  // crypto.randomUUID is available in Node 18+ and every browser that has
  // fetch, so it needs no polyfill in any environment this SDK supports.
  return globalThis.crypto.randomUUID();
}
