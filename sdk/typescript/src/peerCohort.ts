// Peer-cohort types + client. Mirrors `airlayer::engine::cohort::PeerCohortResult`
// over the `/<project_id>/semantic/cohort` HTTP endpoint. Serde emits snake_case
// so these field names match the wire format verbatim.

import type { OxyConfig } from "./config";

// ── Cohort ───────────────────────────────────────────────────────────────────

/** How a subject's benchmark is computed from its peers. Serde snake_case:
 *  `"median"` | `"p75"` | `"best_peer"`. */
export type BenchmarkStatistic = "median" | "p75" | "best_peer";

/**
 * Why a subject was left out of the comparison entirely — distinct from
 * `sufficient: false` on a {@link CohortSubject}, which still gets a computed
 * comparison. This is the fix for a reference implementation where an excluded
 * store "simply did not appear in the list and no screen said why": every
 * exclusion here carries a reason, and a consumer MUST render these rather
 * than silently drop them.
 */
export interface ExcludedSubject {
  /** For a NULL entity key, a synthesized stable id of the form
   *  `"(null) [<require values>]"` rather than an empty string. */
  key: string;
  /** Human-readable prose explaining the exclusion — NOT a coded enum, so
   *  render it verbatim rather than switching on it. */
  reason: string;
}

/**
 * One subject's comparison against its peer cohort.
 *
 * `sufficient` reflects whether `peer_count` met the cohort's declared
 * `min_peers`, but `min_peers` is a REPORTING predicate, never a filter: a
 * subject below the threshold is still returned with `baseline` and `gap`
 * computed from whatever peers it has. Never filter on `sufficient`
 * client-side — surface it so the caller can decide whether to act on a
 * comparison built from a thin peer set.
 */
export interface CohortSubject {
  key: string;
  value: number;
  /** The peer benchmark value. `0.0` when `peer_count === 0` — this is NOT a
   *  baseline of zero, there simply is none. Check `peer_count` before
   *  treating `baseline` as meaningful. */
  baseline: number;
  /** Oriented so a positive value always means opportunity, regardless of
   *  whether the underlying measure is "bigger is better" or the reverse.
   *  `0.0` when there are no peers to compare against. */
  gap: number;
  /** Non-reciprocal: appearing in another subject's `peers` does not imply
   *  this subject lists them back. */
  peers: string[];
  peer_count: number;
  /** `peer_count >= cohort's min_peers`. A reporting flag, not a gate — see
   *  the type-level doc above. */
  sufficient: boolean;
}

/**
 * Result of resolving one subject's peer cohort and every peer's comparison
 * against it.
 */
export interface PeerCohortResult {
  entity: string;
  cohort: string;
  measure: string;
  statistic: BenchmarkStatistic;
  /** `[start, end]` inclusive date strings for the measured period. */
  period: [string, string];
  /**
   * The window peers were drawn from, when the cohort declares a peer band.
   * `Some(period)` when the cohort declares a band with no explicit
   * `window:` of its own; absent only when the cohort declares no band at
   * all. Anchored at `period`'s START and extended backward, so it always
   * CONTAINS `period` — never a disjoint comparison window.
   *
   * `| null` because this mirrors an `Option<(String, String)>` on a
   * GIT-PINNED struct: `skip_serializing_if` is a serde attribute today, not
   * a guarantee, so a reader must accept both encodings. Compare with
   * `!= null`, never `!== undefined`.
   */
  band_window?: [string, string] | null;
  subjects: CohortSubject[];
  excluded: ExcludedSubject[];
}

/**
 * Request to resolve a peer cohort. `cohort` and `band_window` are echoed
 * back on {@link PeerCohortResult} specifically so a UI rendering them cannot
 * drift from the query that produced them.
 */
export interface CohortRequest {
  /**
   * The bare name of an `entities:` entry in the semantic model — e.g.
   * `"restaurant_id"` — NOT a column name and NOT a qualified `view.entity`
   * (`"stores.store"`). The server compares this verbatim against the
   * entity half of a measure's `default_cohort: "entity.cohort_name"` and
   * against `Entity::name` when locating the bound view; a qualified name
   * fails both checks and a scoped caller gets `403 cohort_scope_unavailable`.
   */
  entity: string;
  measure: string;
  time_dimension: string;
  /** `[start, end]` inclusive date strings. */
  period: [string, string];
  /** Falls back to the measure's `default_cohort:` when omitted. */
  cohort?: string;
  /** Defaults to `"median"` server-side when omitted. */
  statistic?: string;
}

// ── Client ───────────────────────────────────────────────────────────────────

/**
 * Shape of the inner request helper exposed by `OxyClient`. The peer-cohort
 * client reuses it to inherit auth headers, timeout, baseUrl, and project
 * scoping rather than reimplementing fetch end-to-end.
 */
export type RequestFn = <T>(endpoint: string, options?: RequestInit) => Promise<T>;

/**
 * Client for the `/semantic/cohort` endpoint. Surfaces airlayer's peer-cohort
 * benchmarking — comparing an entity's subjects against their declared peers
 * on a measure, with exclusions explained rather than silently dropped.
 *
 * Construction is internal to {@link OxyClient} — call `client.peerCohort`
 * to access an instance rather than building one yourself.
 *
 * @example
 * ```typescript
 * const client = await OxyClient.create({ projectId: "...", apiKey: "..." });
 * const result = await client.peerCohort.resolve({
 *   entity: "restaurant_id",
 *   measure: "orders.net_revenue",
 *   time_dimension: "orders.order_date",
 *   period: ["2025-09-01", "2025-09-30"],
 * });
 * for (const excluded of result.excluded) {
 *   console.warn(excluded.key, excluded.reason);
 * }
 * ```
 */
export class PeerCohortClient {
  private readonly request: RequestFn;
  private readonly config: OxyConfig;

  constructor(config: OxyConfig, request: RequestFn) {
    this.config = config;
    this.request = request;
  }

  private path(suffix: string): string {
    return `/${this.config.projectId}${suffix}`;
  }

  private buildQuery(extra: Record<string, string> = {}): string {
    const params: Record<string, string> = { ...extra };
    if (this.config.branch) params.branch = this.config.branch;
    const qs = new URLSearchParams(params).toString();
    return qs ? `?${qs}` : "";
  }

  /**
   * Resolve a subject's peer cohort and every peer's comparison against it.
   *
   * @example
   * ```typescript
   * const result = await client.peerCohort.resolve({
   *   entity: "restaurant_id",
   *   measure: "orders.net_revenue",
   *   time_dimension: "orders.order_date",
   *   period: ["2025-09-01", "2025-09-30"],
   * });
   * ```
   */
  async resolve(request: CohortRequest): Promise<PeerCohortResult> {
    const query = this.buildQuery();
    return this.request<PeerCohortResult>(this.path(`/semantic/cohort${query}`), {
      method: "POST",
      body: JSON.stringify(request)
    });
  }
}
