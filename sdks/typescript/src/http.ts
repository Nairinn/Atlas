import { AtlasError, AtlasConnectionError } from './errors.js';

export interface HttpOptions {
  baseUrl: string;
  /** Per-request timeout in milliseconds. */
  timeoutMs: number;
  /** Attempts after the first. 0 disables retries. */
  maxRetries: number;
  /** Injectable for tests; defaults to global fetch. */
  fetch?: typeof globalThis.fetch;
  /** Extra headers sent on every request. */
  headers?: Record<string, string>;
}

export interface RequestOptions {
  method: 'GET' | 'POST' | 'DELETE';
  path: string;
  query?: Record<string, string | number | boolean | undefined>;
  body?: unknown;
  /** Bearer token for this request, if any. */
  token?: string | undefined;
  headers?: Record<string, string>;
  signal?: AbortSignal | undefined;
}

/**
 * The transport: one place that knows about URLs, auth headers, the error
 * envelope, timeouts, and retries.
 *
 * # Retry policy
 *
 * Only requests that are safe to repeat are retried:
 *
 *   * `GET` and `DELETE` are idempotent by definition. `DELETE` on a
 *     geofence is a soft-delete that returns 404 the second time, which is
 *     a correct answer rather than a corruption.
 *   * `POST` is retried **only** when it carries an `Idempotency-Key`.
 *     Blindly repeating a POST is how you charge someone twice, and this
 *     API has exactly one POST that moves money.
 *
 * That is why `payments.createTransaction` always sends an idempotency
 * key: without one it would be the only unretryable call in the SDK.
 */
export class Http {
  private readonly baseUrl: string;
  private readonly timeoutMs: number;
  private readonly maxRetries: number;
  private readonly doFetch: typeof globalThis.fetch;
  private readonly baseHeaders: Record<string, string>;

  constructor(opts: HttpOptions) {
    // Trailing slashes would produce `//v1/...`, which some proxies
    // normalise and others 404.
    this.baseUrl = opts.baseUrl.replace(/\/+$/, '');
    this.timeoutMs = opts.timeoutMs;
    this.maxRetries = opts.maxRetries;
    this.doFetch = opts.fetch ?? globalThis.fetch;
    this.baseHeaders = opts.headers ?? {};
  }

  async request<T>(opts: RequestOptions): Promise<T> {
    const url = this.buildUrl(opts.path, opts.query);
    const idempotent =
      opts.method === 'GET' ||
      opts.method === 'DELETE' ||
      Boolean(opts.headers?.['Idempotency-Key']);
    const attempts = idempotent ? this.maxRetries + 1 : 1;

    let lastError: unknown;
    for (let attempt = 0; attempt < attempts; attempt++) {
      if (attempt > 0) {
        // Exponential backoff with jitter. Without jitter, every client
        // that saw the same blip retries in lockstep and re-creates it.
        // A Retry-After the server sent (429) overrides the guess,
        // capped so a hostile value cannot stall the caller.
        const base = 100 * 2 ** (attempt - 1);
        const hinted =
          lastError instanceof AtlasError ? lastError.retryAfterMs : undefined;
        const wait = Math.max(base, hinted ?? 0) + Math.random() * base;
        await sleep(wait);
      }
      try {
        return await this.attempt<T>(url, opts);
      } catch (err) {
        lastError = err;
        const retryable =
          err instanceof AtlasConnectionError ||
          (err instanceof AtlasError && err.retryable);
        if (!retryable) throw err;
      }
    }
    throw lastError;
  }

  private async attempt<T>(url: string, opts: RequestOptions): Promise<T> {
    // Compose the caller's signal with the timeout so an explicit abort
    // still works and neither leaks a timer.
    const timeout = AbortSignal.timeout(this.timeoutMs);
    const signal = opts.signal
      ? AbortSignal.any([opts.signal, timeout])
      : timeout;

    const headers: Record<string, string> = {
      accept: 'application/json',
      ...this.baseHeaders,
      ...opts.headers,
    };
    if (opts.token) headers['authorization'] = `Bearer ${opts.token}`;
    if (opts.body !== undefined) headers['content-type'] = 'application/json';

    let response: Response;
    try {
      response = await this.doFetch(url, {
        method: opts.method,
        headers,
        // Spread rather than `body: undefined`: exactOptionalPropertyTypes
        // treats an explicit undefined as a real value, and RequestInit
        // does not accept one.
        ...(opts.body === undefined ? {} : { body: JSON.stringify(opts.body) }),
        signal,
      });
    } catch (cause) {
      // No response at all. Deliberately a distinct type from AtlasError:
      // a caller that wants to know "did the server see this request"
      // cannot answer that from a status code.
      throw new AtlasConnectionError(
        `request to ${opts.path} failed`,
        opts.path,
        cause,
      );
    }

    if (response.status === 204) return undefined as T;

    const text = await response.text();
    if (!response.ok) {
        throw toError(
          response.status,
          text,
          opts.path,
          parseRetryAfterMs(response.headers.get('retry-after')),
        );
      }
    if (text.length === 0) return undefined as T;

    try {
      return JSON.parse(text) as T;
    } catch (cause) {
      throw new AtlasConnectionError(
        `malformed JSON in response from ${opts.path}`,
        opts.path,
        cause,
      );
    }
  }

  private buildUrl(
    path: string,
    query?: Record<string, string | number | boolean | undefined>,
  ): string {
    const url = new URL(this.baseUrl + path);
    for (const [key, value] of Object.entries(query ?? {})) {
      // Omit undefined rather than sending "undefined", which the gateway
      // would reject as an unparseable number.
      if (value !== undefined) url.searchParams.set(key, String(value));
    }
    return url.toString();
  }
}

function toError(
  status: number,
  text: string,
  path: string,
  retryAfterMs?: number | undefined,
): AtlasError {
  // The envelope is the contract, but a proxy or load balancer can return
  // an HTML error page that never reached the gateway. Fall back rather
  // than throwing a JSON parse error that hides the real status.
  let code = 'internal';
  let message = text.slice(0, 200) || `request failed with status ${status}`;
  try {
    const parsed = JSON.parse(text) as { error?: { code?: string; message?: string } };
    if (parsed.error?.code) code = parsed.error.code;
    if (parsed.error?.message) message = parsed.error.message;
  } catch {
    // Not our envelope; keep the raw text.
  }
  return new AtlasError({
    code,
    message,
    status,
    path,
    ...(retryAfterMs !== undefined ? { retryAfterMs } : {}),
  });
}

/** The longest a server hint may delay a retry. */
const MAX_RETRY_AFTER_MS = 30_000;

function parseRetryAfterMs(header: string | null): number | undefined {
  if (header === null) return undefined;
  const seconds = Number(header);
  if (Number.isFinite(seconds) && seconds >= 0) {
    return Math.min(seconds * 1000, MAX_RETRY_AFTER_MS);
  }
  // HTTP-date form: trust only if it parses, cap the distance.
  const at = Date.parse(header);
  if (!Number.isNaN(at)) {
    return Math.max(0, Math.min(at - Date.now(), MAX_RETRY_AFTER_MS));
  }
  return undefined;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
