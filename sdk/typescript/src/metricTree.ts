// Metric-tree types + client. Mirrors `airlayer::engine::metric_tree*`
// over the `/<project_id>/semantic/metric-tree*` HTTP endpoints. Serde
// emits snake_case so these field names match the wire format verbatim.

import type { OxyConfig } from "./config";
import type { BenchmarkStatistic } from "./peerCohort";

// ── Tree ──────────────────────────────────────────────────────────────────────

export type EdgeKind = "component" | "driver";
export type DriverDirection = "positive" | "negative" | "unknown";
export type DriverStrength = "strong" | "moderate" | "weak";
export type DriverConfidence = "high" | "medium" | "low";
/** The shape of a driver relationship.
 *
 *  The THIRD hand-maintained mirror of this enum (airlayer's is canonical,
 *  `web-app/src/types/metricTree.ts` is the second). Nothing enforces that they
 *  agree, and the last time one fell behind — `oxy-semantic`, five variants
 *  short — a valid `.view.yml` stopped parsing. A type-only union fails more
 *  softly: an SDK consumer reading a tree with a quadratic edge just gets a
 *  union that cannot hold it. Add new shapes here whenever airlayer grows one. */
export type DriverForm =
  | "linear"
  | "log-log"
  | "log-linear"
  | "linear-log"
  | "quadratic"
  | "cubic"
  | "sqrt"
  | "inverse"
  | "linear-log-quadratic";

export interface MetricNode {
  id: string;
  view: string;
  measure: string;
  label: string;
  description?: string | null;
  measure_type: string;
  is_composite: boolean;
  /** Whether this measure can be drilled into. Serialized rather than
   *  re-derived: `measure_type` misses eligible composites, and edge presence
   *  over-admits nested / cross-view / multiplicative passthroughs the engine
   *  refuses. Non-optional — it is on every metric-tree response. */
  drillable: boolean;
  expr?: string | null;
}

export interface MetricEdge {
  from: string;
  to: string;
  kind: EdgeKind;
  /** Sign of a component edge; omitted (defaults to +1) for most edges. */
  sign?: number;
  /** Arithmetic operator joining a component child to its parent. Omitted
   *  when it is `add` — airlayer skips the field at its default — so absent
   *  MEANS `add`, never "unknown". Only `mul` / `div` are multiplicative;
   *  `add` / `sub` propagate exactly. */
  operator?: "add" | "sub" | "mul" | "div";
  direction: DriverDirection;
  strength: DriverStrength;
  confidence: DriverConfidence;
  coefficient?: number | null;
  form: DriverForm;
  /** Whether `form` was declared in the YAML or inferred by the fit. */
  form_declared?: boolean;
  intercept?: number | null;
  lag?: number | null;
  description?: string | null;
  refs?: string[] | null;
}

export interface MetricTree {
  nodes: MetricNode[];
  edges: MetricEdge[];
  root?: string | null;
  /** Refusals raised while building the tree — a driver declaring both
   *  `coefficient:` and `coefficients:`, or a wrong-width vector. Absent when
   *  empty, so a lever that moves nothing still has a way to say why. */
  warnings?: string[];
}

// ── Sensitivity ──────────────────────────────────────────────────────────────

export interface SensitivityDriver {
  measure: string;
  path: string[];
  edge_kind: string;
  effective_coefficient?: number | null;
  form?: DriverForm | null;
  direction: DriverDirection;
  strength: DriverStrength;
  lag?: number | null;
  description?: string | null;
}

export interface SensitivityResult {
  target: string;
  drivers: SensitivityDriver[];
}

// ── Predict ──────────────────────────────────────────────────────────────────

export interface PredictChange {
  measure: string;
  delta: number;
}

export interface PredictImpact {
  measure: string;
  estimated_delta: number;
  confidence: string;
  path: string[];
  form: DriverForm;
  lag?: number | null;
}

export interface PredictResult {
  inputs: PredictChange[];
  impacts: PredictImpact[];
}

/** node_id → the measure's value over the baseline window. */
export type MeasureValues = Record<string, number>;

/** A driver edge's coefficient, measured from history by the baseline query.
 *
 *  Either `coefficient` is set or `refusal` is — never both, never neither.
 *  A refusal is a result: it is why a measure downstream of the change shows
 *  no number. Echo the whole array into `predict` verbatim, refusals included;
 *  the server ignores entries carrying no coefficient, and filtering them here
 *  would be a second place for the two sides to disagree. */
export interface FittedDriver {
  from: string;
  to: string;
  lag?: number;
  /** The form the slope was measured in. The same number reads as dollars per
   *  dollar under `linear` and as a percent-per-percent elasticity under
   *  `log-log`, so a bare figure has no unit. */
  form?: DriverForm;
  /** Paired observations behind the fit.
   *
   *  `| null` because this mirrors an `Option<f64>` on a GIT-PINNED struct:
   *  `skip_serializing_if` is a serde attribute today, not a guarantee, so a
   *  reader must accept both encodings. Compare with `!= null`, never
   *  `!== undefined` — and the type has to admit both, or the safe read looks
   *  like dead code. Same rule on `t_stat`, `t_stats`, `se_terms` and
   *  `coefficient` below. */
  n?: number | null;
  n_panels?: number;
  n_nonpositive?: number;
  /** The FIRST basis term — the whole answer for a single-term form; for a
   *  shape that can turn it is only the slope, so read `coefficients`.
   *  `| null` because it is an `Option<f64>` on the wire. */
  coefficient?: number | null;
  /** One coefficient per basis term, in basis order. This is what propagation
   *  evaluates. */
  coefficients?: number[];
  se?: number;
  /** Elements `| null` for the reason stated on `n`. */
  se_terms?: (number | null)[];
  /** `| null` for the reason stated on `n`. */
  t_stat?: number | null;
  /** `t` per basis term, in basis order — `[1]` is the second basis term, the
   *  squared one under every shape that can turn. Elements `| null` for the
   *  reason stated on `n`. */
  t_stats?: (number | null)[];
  /** Sufficient statistics of the basis over the rows the fit used. Not
   *  diagnostic: the fit is per row and a change is a window aggregate, and a
   *  curved response cannot cross that gap without these. Echo verbatim. */
  moments?: { n?: number; s1?: number; s2?: number };
  /** `[min, max]` driver values observed. A change beyond this spread is
   *  refused rather than extrapolated. */
  domain?: [number, number];
  /** The response sampled as `[change fraction, delta]`. Read this instead of
   *  interpreting the coefficients — peak, break-even and saturation are all
   *  properties of these samples, so a reader written against them keeps
   *  working when a new shape is added. */
  profile?: [number, number][];
  form_source?: "declared" | "inferred";
  /** Every shape considered, scored comparably (AIC in y-space, lower better).
   *  Empty when the form was declared. `all_terms_significant` false means the
   *  candidate was never eligible, however good its score. */
  candidates?: { form: DriverForm; aic: number; all_terms_significant: boolean }[];
  refusal?: string;
}

/** Why a reachable node has no baseline value. */
export interface UnvaluedNode {
  id: string;
  reason?: string | null;
}

/** Narrow the baseline to one world-model instance. Omit to value the whole
 *  population. */
export interface BaselineInstance {
  entity: string;
  /** JSON array for a composite key, else a bare scalar. */
  key: string;
}

export interface BaselineRequest {
  /** The nodes you intend to change. Values are fetched for these plus
   *  everything forward-reachable from them — not the whole tree. */
  roots: string[];
  time_dimension: string;
  /** `[start, end]` inclusive date strings. */
  period: [string, string];
  instance?: BaselineInstance | null;
}

export interface BaselineResponse {
  values: MeasureValues;
  unvalued: UnvaluedNode[];
  resolved_period: [string, string];
  /** Why the baseline produced no values, in words worth showing. Absent when
   *  measures were valued normally. */
  baseline_note?: string | null;
  /** Coefficients fitted for driver edges that declare none, plus refusals.
   *  Absent when every reachable driver edge already declares one. */
  fitted?: FittedDriver[];
}

export interface PredictOptions {
  /** Current values for the measures involved. Supplying them lets
   *  multiplicative edges be sized instead of returned `unquantifiable`. */
  values?: MeasureValues;
  /** The baseline's `fitted` array, verbatim. */
  coefficients?: FittedDriver[];
}

// ── Projection (scenario forecasting over time) ──────────────────────────────

export type ProjectionGranularity = "day" | "week" | "month";

/**
 * The scenario's time axis. `baseline` answers "what is this measure worth
 * over the window"; this answers "what has it been doing, and what does it do
 * next".
 *
 * The history window is deliberately its own, NOT the baseline's: the
 * forecaster refuses anything under eight seasonal cycles (56 daily buckets,
 * 32 weekly, 24 monthly), and a 30-day scenario baseline reused here would
 * make "no forecast" the normal answer.
 */
export interface ProjectionRequest {
  /** Lever node ids. Curves are drawn for these plus everything
   *  forward-reachable from them — the same set {@link BaselineRequest} values. */
  roots: string[];
  time_dimension: string;
  /** `[start, end]` inclusive date strings for the HISTORY. */
  period: [string, string];
  /** Narrow to one world-model instance. Omit to project the whole
   *  population — same picker the baseline uses. */
  instance?: BaselineInstance | null;
  /** Bucket width. Defaults to `day` server-side. */
  granularity?: ProjectionGranularity;
  /** Buckets to project past the last historical one. 1..=365; outside that
   *  it is a 400, never a silent clamp — a horizon quietly truncated reads as
   *  a forecast that genuinely ends in March. */
  horizon: number;
  /** Seasonal periods, in buckets, applied to every measure in the request.
   *
   *  Omitting it is not "use the default" — it means *resolve per measure*
   *  from whatever `.monitor.yml` already watches that series, which is what
   *  keeps this band the band an anomaly had to breach. Send it only to pin a
   *  cycle nobody has declared. Each period must be >= 2; `[]` is a 400. */
  seasonality?: number[];
}

export interface HistoryPoint {
  /** Bucket start, `YYYY-MM-DD`. */
  date: string;
  value: number;
}

export interface ForecastPoint {
  date: string;
  point: number;
  /** The prediction interval. `null` / absent means the model returned no
   *  band — unknown spread, NOT a band of zero width. Never collapse these
   *  onto `point`: a zero-width band is a claim of certainty nobody made. */
  lower?: number | null;
  upper?: number | null;
}

/** One measure's baseline curve: what happened, then what comes next.
 *
 *  An empty `forecast` carrying a `refusal` is a state, not a gap — most often
 *  "too little history to fit", or the warehouse refusing this one measure. It
 *  must never render as a flat forward line, which is what any code defaulting
 *  the missing curve to "unchanged" would draw. */
export interface MeasureProjection {
  measure: string;
  history: HistoryPoint[];
  forecast: ForecastPoint[];
  refusal?: string | null;
  /** The seasonal periods this curve was decomposed against — resolved per
   *  measure, so two series in one response can legitimately differ. */
  seasonality: number[];
}

export interface ProjectionResponse {
  granularity: ProjectionGranularity;
  /** Echoed back: the query is expensive and callers cache on it. */
  resolved_period: [string, string];
  horizon: number;
  series: MeasureProjection[];
  /** Why the WHOLE projection is empty, when it is. Absent when at least one
   *  measure produced history — a partial failure is each measure's own
   *  `refusal`, never a banner over curves that are drawing fine. */
  projection_note?: string | null;
}

// ── Explain (RCA) ────────────────────────────────────────────────────────────

export type SplitKind =
  | { type: "component"; child_measure: string }
  | { type: "dimension"; dimension: string; value: string }
  | { type: "uniform_degradation"; dimension: string; num_elements: number }
  | { type: "cross_cutting"; dimension: string; value: string; measures: string[] };

export interface ExplainSibling {
  split: SplitKind;
  measure: string;
  delta: number;
  root_fraction: number;
}

export interface ExplainNode {
  split: SplitKind;
  measure: string;
  filters: unknown[];
  delta: number;
  concentration: number;
  root_fraction: number;
  siblings?: ExplainSibling[];
  dimension_count?: number;
  children?: ExplainNode[];
}

/** Whether a driver's observed move pushes the target the way it actually
 *  moved (`contributing`) or against it (`counteracting` — it offset part of
 *  the move rather than causing it). `unknown` when no signed claim is
 *  available: `direction: unknown` with no coefficient, or a flat
 *  driver/target. */
export type DriverContribution = "contributing" | "counteracting" | "unknown";

/** A driver's move split into the part its base forced and the part its own
 *  ratio contributed. Emitted only when the driver genuinely tracks a sibling
 *  rather than moving on its own — presence is the claim.
 *  `base_driven_delta + ratio_driven_delta === driver_delta`. */
export interface PassthroughSplit {
  base_measure: string;
  ratio_previous: number;
  ratio_current: number;
  base_driven_delta: number;
  ratio_driven_delta: number;
}

export interface DriverAttribution {
  driver_measure: string;
  driver_previous: number;
  driver_current: number;
  driver_delta: number;
  /** Both optional: an `explain_cache` row written before these fields shipped
   *  is served verbatim, so absent means unclassified — not a default. */
  direction?: DriverDirection;
  contribution?: DriverContribution;
  coefficient?: number;
  form: DriverForm;
  /** Absent for a purely qualitative driver (no coefficient). */
  estimated_target_impact?: number;
  description?: string;
  passthrough?: PassthroughSplit;
}

export type ExplainWarning =
  | {
      type: "simpsons_paradox";
      dimension: string;
      aggregate_delta: number;
      segment_directions: [string, number][];
    }
  | {
      type: "opposing_offset";
      component_a: string;
      component_b: string;
      delta_a: number;
      delta_b: number;
    }
  | {
      type: "non_additive_dimension_split";
      measure: string;
      measure_type: string;
      dimension: string;
    };

export interface ExplainConfigOverride {
  deep?: boolean;
  max_depth?: number;
  coverage_threshold?: number;
}

export interface ExplainRequest {
  target: string;
  time_dimension: string;
  current_period: [string, string];
  previous_period: [string, string];
  config?: ExplainConfigOverride;
}

export interface ExplainResult {
  target: string;
  target_delta: number;
  target_previous: number;
  target_current: number;
  time_dimension: string;
  current_period: [string, string];
  previous_period: [string, string];
  nodes: ExplainNode[];
  coverage: number;
  driver_attribution?: DriverAttribution[];
  alternatives?: unknown[];
  warnings?: ExplainWarning[];
}

// ── Opportunity ──────────────────────────────────────────────────────────────

export interface SegmentOpportunity {
  segment: string;
  current_value: number;
  volume: number;
  benchmark: number;
  gap: number;
  /** Match-the-best upside in measure units. */
  upside: number;
}

export interface DimensionOpportunity {
  dimension: string;
  cardinality: number;
  /** "best_peer" or "p75". */
  benchmark_basis: string;
  total_upside: number;
  segments: SegmentOpportunity[];
  other_segments_skipped: number;
}

export interface SkippedDimension {
  dimension: string;
  reason: string;
}

export interface OpportunityRequest {
  target: string;
  time_dimension: string;
  period: [string, string];
  /**
   * How a segment's benchmark is computed from its peers. Defaults to
   * `"p75"` server-side when omitted.
   *
   * An airlayer upgrade deleted the engine's adaptive selection — best-peer
   * for a dimension with too few segments for a percentile to mean anything,
   * p75 once there were enough — in favor of an explicit required argument.
   * No single fixed value reproduced the old behavior, so rather than
   * freezing one server-side, the choice is now the caller's.
   */
  statistic?: BenchmarkStatistic;
}

export interface OpportunityResult {
  target: string;
  period: [string, string];
  overall_value: number;
  /**
   * "rows" (rate-based additive sizing — the only basis that yields a sized
   * upside figure), "value_share" (additive) or "equal" (ratios).
   */
  weight_basis: string;
  dimensions: DimensionOpportunity[];
  skipped_dimensions: SkippedDimension[];
  downstream: PredictImpact[];
}

// ── Distribution ─────────────────────────────────────────────────────────────

/**
 * Single-period structural decomposition. The server auto-derives the
 * baseline as the equal-length window immediately before `period`, then
 * returns an {@link ExplainResult}-shaped payload (so the same renderers
 * work). Ignore the delta fields when rendering a pure distribution.
 */
export interface DistributionRequest {
  target: string;
  time_dimension: string;
  /** `[start, end]` inclusive date strings. */
  period: [string, string];
}

// ── Time dimensions ──────────────────────────────────────────────────────────

export interface TimeDimensionsResponse {
  /** view name → fully-qualified time-dimension ids (`view.dim`). */
  by_view: Record<string, string[]>;
}

// ── Client ───────────────────────────────────────────────────────────────────

/**
 * Shape of the inner request helper exposed by `OxyClient`. The metric-tree
 * client reuses it to inherit auth headers, timeout, baseUrl, and project
 * scoping rather than reimplementing fetch end-to-end.
 */
export type RequestFn = <T>(endpoint: string, options?: RequestInit) => Promise<T>;

/**
 * Client for the `/semantic/metric-tree*` endpoints. Surfaces the airlayer
 * metric-tree analyses — tree introspection, sensitivity, explain, opportunity
 * — plus the three legs of scenario forecasting (`baseline` levels,
 * `predict` propagation, `projection` curves) over typed methods.
 *
 * Construction is internal to {@link OxyClient} — call `client.metricTree`
 * to access an instance rather than building one yourself.
 *
 * @example
 * ```typescript
 * const client = await OxyClient.create({ projectId: "...", apiKey: "..." });
 * const tree = await client.metricTree.getTree();
 * const drivers = await client.metricTree.getSensitivity("orders.net_revenue");
 * ```
 */
export class MetricTreeClient {
  private readonly request: RequestFn;
  private readonly config: OxyConfig;

  constructor(config: OxyConfig, request: RequestFn) {
    this.config = config;
    this.request = request;
  }

  private path(suffix: string): string {
    return `/${this.config.projectId}/semantic/metric-tree${suffix}`;
  }

  private buildQuery(extra: Record<string, string> = {}): string {
    const params: Record<string, string> = { ...extra };
    if (this.config.branch) params.branch = this.config.branch;
    const qs = new URLSearchParams(params).toString();
    return qs ? `?${qs}` : "";
  }

  /**
   * Fetch the full metric tree, or the subtree rooted at `root`.
   *
   * @param root - Optional fully-qualified measure id to root the tree at.
   * @returns Nodes (measures) and edges (component / driver relationships).
   *
   * @example
   * ```typescript
   * const tree = await client.metricTree.getTree();
   * const subtree = await client.metricTree.getTree("orders.net_revenue");
   * ```
   */
  async getTree(root?: string): Promise<MetricTree> {
    const query = this.buildQuery(root ? { root } : {});
    return this.request<MetricTree>(this.path(query));
  }

  /**
   * Rank the declared drivers of a measure by influence.
   *
   * @param measureId - Fully-qualified measure id (`view.measure`).
   *
   * @example
   * ```typescript
   * const sensitivity = await client.metricTree.getSensitivity("orders.net_revenue");
   * for (const driver of sensitivity.drivers) {
   *   console.log(driver.measure, driver.direction, driver.strength);
   * }
   * ```
   */
  async getSensitivity(measureId: string): Promise<SensitivityResult> {
    const query = this.buildQuery();
    return this.request<SensitivityResult>(
      this.path(`/${encodeURIComponent(measureId)}/sensitivity${query}`)
    );
  }

  /**
   * Value a change's starting point, and measure the coefficients it needs.
   *
   * Two warehouse reads: the current value of every node reachable from
   * `roots`, and — for driver edges that declare no `coefficient:` — a fit
   * over the window. Both are expensive, which is why they live here and not
   * in `predict`: `predict` is database-free by design so it can re-run per
   * keystroke, and it CANNOT measure a coefficient itself.
   *
   * That is the whole reason to call this. Pass `fitted` back into `predict`
   * and an undeclared edge propagates; omit it and `predict` has nothing to
   * multiply by, so the impact is simply absent — no error, no refusal, just a
   * downstream measure that never appears.
   *
   * @example
   * ```typescript
   * const baseline = await client.metricTree.getBaseline({
   *   roots: ["marketing_spend.total_spend"],
   *   time_dimension: "orders.order_date",
   *   period: ["2025-09-01", "2025-09-30"],
   * });
   * const result = await client.metricTree.predict(
   *   [{ measure: "marketing_spend.total_spend", delta: 10000 }],
   *   { values: baseline.values, coefficients: baseline.fitted }
   * );
   * ```
   */
  async getBaseline(request: BaselineRequest): Promise<BaselineResponse> {
    const query = this.buildQuery();
    return this.request<BaselineResponse>(this.path(`/baseline${query}`), {
      method: "POST",
      body: JSON.stringify(request)
    });
  }

  /**
   * Propagate hypothetical `(measure, delta)` changes upward through the
   * tree. Returns the estimated impact on every downstream measure.
   *
   * Database-free, so it re-runs cheaply — and so it can only use
   * coefficients it is GIVEN. Without `options.coefficients` from
   * {@link getBaseline}, every edge whose `.view.yml` declares no
   * `coefficient:` contributes nothing and its downstream measures are
   * silently missing from `impacts`. Without `options.values`, multiplicative
   * component edges come back `unquantifiable` rather than sized.
   *
   * @example
   * ```typescript
   * const result = await client.metricTree.predict([
   *   { measure: "marketing_spend.total_spend", delta: 10000 },
   * ]);
   * ```
   */
  async predict(changes: PredictChange[], options: PredictOptions = {}): Promise<PredictResult> {
    const query = this.buildQuery();
    return this.request<PredictResult>(this.path(`/predict${query}`), {
      method: "POST",
      body: JSON.stringify({
        changes,
        ...(options.values ? { values: options.values } : {}),
        // Sent verbatim, refusals included — the server ignores entries
        // carrying no coefficient, and filtering them here would just be a
        // second place for the two sides to disagree.
        ...(options.coefficients?.length ? { coefficients: options.coefficients } : {})
      })
    });
  }

  /**
   * Draw the scenario's time axis: bucketed history for the levers and
   * everything downstream, plus the forward curve the detector's own model
   * expects next.
   *
   * The third leg of scenario forecasting. {@link getBaseline} gives levels
   * and coefficients, {@link predict} propagates a change with no database at
   * all, and this gives time — one warehouse query, so treat it like the
   * baseline: fetch on a window change, not on a lever edit.
   *
   * **Returns the BASELINE curve only.** The scenario's second curve is
   * arithmetic over this and a `predict` result — a proportional shift landing
   * `lag` buckets in — and is composed client-side deliberately, so editing a
   * lever costs no query.
   *
   * @example
   * ```typescript
   * const projection = await client.metricTree.getProjection({
   *   roots: ["marketing_spend.total_spend"],
   *   time_dimension: "orders.order_date",
   *   period: ["2024-09-01", "2025-08-31"],
   *   granularity: "day",
   *   horizon: 30,
   * });
   * for (const series of projection.series) {
   *   if (series.refusal) console.warn(series.measure, series.refusal);
   * }
   * ```
   */
  async getProjection(request: ProjectionRequest): Promise<ProjectionResponse> {
    const query = this.buildQuery();
    return this.request<ProjectionResponse>(this.path(`/projection${query}`), {
      method: "POST",
      body: JSON.stringify(request)
    });
  }

  /**
   * Period-over-period root-cause decomposition. Recursively splits the
   * target measure by components and dimensions until the move concentrates.
   *
   * @example
   * ```typescript
   * const result = await client.metricTree.explain({
   *   target: "financials.operating_profit",
   *   time_dimension: "financials.month",
   *   current_period: ["2025-09-01", "2025-09-30"],
   *   previous_period: ["2025-08-01", "2025-08-31"],
   * });
   * ```
   */
  async explain(request: ExplainRequest): Promise<ExplainResult> {
    const query = this.buildQuery();
    return this.request<ExplainResult>(this.path(`/explain${query}`), {
      method: "POST",
      body: JSON.stringify(request)
    });
  }

  /**
   * Size the upside opportunity for a measure by finding underperforming
   * segments. Skips high-cardinality dimensions and trims the long tail.
   *
   * @example
   * ```typescript
   * const result = await client.metricTree.findOpportunities({
   *   target: "orders.net_revenue",
   *   time_dimension: "orders.order_date",
   *   period: ["2025-09-01", "2025-09-30"],
   * });
   * for (const dim of result.dimensions) {
   *   console.log(dim.dimension, "+", dim.total_upside);
   * }
   * ```
   */
  async findOpportunities(request: OpportunityRequest): Promise<OpportunityResult> {
    const query = this.buildQuery();
    return this.request<OpportunityResult>(this.path(`/opportunity${query}`), {
      method: "POST",
      body: JSON.stringify(request)
    });
  }
}
