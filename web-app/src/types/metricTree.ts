import type { WmBreakdownEdge } from "./worldModel";

// Metric tree types — mirror the airlayer structs serialized by the
// `/semantic/metric-tree*` endpoints (snake_case, as serde emits them).

export type EdgeKind = "component" | "driver";
export type DriverDirection = "positive" | "negative" | "unknown";
export type DriverStrength = "strong" | "moderate" | "weak";
export type DriverConfidence = "high" | "medium" | "low";
/** The shape of a driver relationship. Each maps to a basis of transformed
 *  regressors plus a link on the target (airlayer `engine::response`), which is
 *  why adding one is a table row rather than a new code path.
 *
 *  Widths differ: `quadratic` and `linear-log-quadratic` need two coefficients
 *  and `cubic` needs three, which is why `coefficients` exists alongside the
 *  scalar. Only those three can TURN AROUND.
 *
 *  Nothing in the UI should switch on a member of this union — the response
 *  profile and the sampled deltas describe every shape without naming it, which
 *  is what lets a new shape land here without a matching UI change. The one
 *  exception is `FORM_HELP` tooltip copy. */
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
  expr?: string | null;
  /** Whether the drill will size this measure on a per-unit rate — airlayer's
   *  `supports_rate_basis(layer, target)`, computed once at tree build and
   *  serialized here so consumers don't re-derive eligibility from
   *  `measure_type` or component-edge presence (both get it wrong: a `type:
   *  sum` check misses eligible composites, and edge presence over-admits
   *  nested/cross-view/multiplicative passthroughs the engine refuses). */
  drillable: boolean;
}

export interface MetricEdge {
  from: string;
  to: string;
  kind: EdgeKind;
  /** Sign of a component edge; omitted (defaults to +1) for most edges. */
  sign?: number;
  /** Arithmetic operator joining a component child to its parent. Omitted when
   *  it is `add` — airlayer skips the field at its default — so absent MEANS
   *  `add`, never "unknown". Only `mul` / `div` are multiplicative; `add` /
   *  `sub` propagate exactly, which is the distinction a "can't size" render
   *  is allowed to depend on. `OP_GLYPH` in `WorldModel/worldModelLayout.ts`
   *  maps the same union to glyphs, and is an exactly-keyed Record — so index
   *  it as `OP_GLYPH[edge.operator ?? "add"]`. */
  operator?: WmBreakdownEdge["operator"];
  direction: DriverDirection;
  strength: DriverStrength;
  confidence: DriverConfidence;
  coefficient?: number | null;
  /** The RESOLVED shape. For an edge that declared none, this is what the fit
   *  selected from history. */
  form: DriverForm;
  /** Whether `form` was declared in the YAML. False means it was inferred, so it
   *  can change as the window moves. */
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
  /** Edges the tree builder could not resolve to a shape, with the reason.
   *
   *  A driver declaring both `coefficient:` and `coefficients:`, or a vector
   *  of the wrong width for its `form:`, stays qualitative — refused WITH a
   *  reason, deliberately, so the author isn't left with a lever that moves
   *  nothing and no way to learn why. Absent from this type, that reason
   *  reached nobody. `skip_serializing_if` on the wire, hence optional. */
  warnings?: string[];
}

// ── sensitivity ─────────────────────────────────────────────────────────────

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

// ── predict ─────────────────────────────────────────────────────────────────

export interface PredictInput {
  measure: string;
  delta: number;
}

/** How much of a claim the engine is making about an impact's magnitude.
 *  Mirrors the three strings `airlayer::engine::metric_tree_ops` emits:
 *
 *  - `exact` — every hop into the measure was an additive component identity
 *    (`Δparent = sign × Δchild`). Arithmetic, not a forecast.
 *  - `estimated` — some hop crossed a driver coefficient or a multiplicative
 *    edge linearized at the current value: a first-order approximation. Once a
 *    path is estimated it stays estimated; a later exact hop cannot restore
 *    confidence an earlier approximation already spent.
 *  - `unquantifiable` — real but unsizable, emitted with `estimated_delta: 0.0`
 *    where the 0 means UNKNOWN. Never render it as a number. */
export type ImpactConfidence = "exact" | "estimated" | "unquantifiable";

export interface PredictImpact {
  measure: string;
  estimated_delta: number;
  confidence: ImpactConfidence;
  path: string[];
  form: DriverForm;
  lag?: number | null;
}

export interface PredictResult {
  inputs: PredictInput[];
  impacts: PredictImpact[];
}

// ── baseline (scenario simulation) ──────────────────────────────────────────

/** node_id → current value over the baseline window. */
export type MeasureValues = Record<string, number>;

export type UnvaluedReason =
  | "no_rows_in_window"
  | "query_failed"
  | "no_matching_columns"
  // No query was issued for this node's view at all — the window or the scope
  // could not be expressed against it. The baseline note carries the reason.
  | "not_queried";

export interface UnvaluedNode {
  node_id: string;
  reason: UnvaluedReason;
}

/** A world-model instance to narrow the baseline to. */
export interface BaselineInstance {
  entity: string;
  /** JSON array for a composite key, else a bare scalar. */
  key: string;
}

export interface BaselineRequest {
  roots: string[];
  time_dimension: string;
  /** `[start, end]` inclusive date strings. */
  period: [string, string];
  instance?: BaselineInstance | null;
}

/** A driver edge's coefficient, measured from history by the baseline query.
 *
 *  Either `coefficient` is set or `refusal` is — never both, never neither.
 *  A refusal is a result, not an absence: it is the reason a measure
 *  downstream of the pinned lever shows no number, and showing it is what
 *  separates "the model declines to guess" from "the UI forgot to render". */
export interface FittedDriver {
  from: string;
  to: string;
  /** Days of lag the pairs were built at. */
  lag?: number;
  /** The functional form the slope was measured in — the edge's declared
   *  `form:`. Load-bearing for display: the same number reads as dollars per
   *  dollar under `linear` and as a percent-per-percent elasticity under
   *  `log-log`, so a bare figure has no unit. Absent on a fit produced before
   *  the server carried it, which could only have been `linear`. */
  form?: DriverForm;
  /** Paired observations behind the fit.
   *
   *  `| null` for the same reason `coefficient` below carries it: this mirrors a
   *  GIT-PINNED struct, so `skip_serializing_if` is a serde attribute today and
   *  not a guarantee. Reads compare with `!= null` so both encodings mean the
   *  same thing — and the type has to admit both, or the safe read looks like
   *  dead code to the next person. */
  n?: number | null;
  /** Entities those observations spanned. */
  n_panels?: number;
  /** Pairs dropped because a logged axis held a non-positive value (a closed
   *  day has no log). Worth showing: it lowers `n`, and `n` is what the
   *  observation gate reads, so it can be the whole reason a window that looks
   *  ample was refused. Always 0 for `linear`. */
  n_nonpositive?: number;
  /** The headline coefficient — the FIRST basis term. The whole answer for every
   *  single-term form; for `quadratic` it is only the slope, so read
   *  `coefficients` when the form can turn.
   *
   *  `| null` because it is an `Option<f64>` on the wire: it is skipped today
   *  (`skip_serializing_if`) but that is a serde attribute on a git-pinned
   *  struct, not a guarantee. Every read compares with `!= null` so both
   *  encodings mean the same thing — and the type has to admit both, or the
   *  test that pins that behaviour cannot be written. */
  coefficient?: number | null;
  /** One coefficient per basis term, in basis order. `quadratic` is
   *  `[slope, curvature]`. This is what propagation evaluates. */
  coefficients?: number[];
  se?: number;
  /** Elements `| null` for the reason stated on `n`: these mirror a
   *  `Vec<Option<f64>>`-shaped payload on a git-pinned struct, and a reader
   *  guarding with `!= null` should not need a cast to express the case. */
  se_terms?: (number | null)[];
  /** `| null` for the reason stated on `n`. */
  t_stat?: number | null;
  /** `t` per basis term. Every one must clear |t| >= 2 or the fit is refused, so
   *  a reader checks `t_stats[1]` to see whether a curvature is real rather than
   *  a peak assembled from noise.
   *
   *  Elements `| null` for the reason stated on `n` — the guard in
   *  `curvatureNote` is `== null`, and the type has to admit that or the safe
   *  branch looks like dead code and the test needs a cast to reach it. */
  t_stats?: (number | null)[];
  /** Sufficient statistics of the basis over the rows the fit used. Load-bearing,
   *  not diagnostic: the fit is per row and a lever is a window aggregate, and a
   *  curved response cannot cross that gap without these. Must be echoed back to
   *  `predict` verbatim along with the coefficients. */
  moments?: { n?: number; s1?: number; s2?: number };
  /** `[min, max]` driver values observed. A lever beyond this spread is refused
   *  rather than extrapolated — a quadratic diverges outside its own evidence. */
  domain?: [number, number];
  /** The response sampled as `[lever fraction, delta]` over the range the fit has
   *  evidence for.
   *
   *  Read this instead of interpreting the coefficients. Peak, break-even,
   *  saturation and the impact of any given move are all properties of these
   *  samples, so a surface written against them keeps working when a new shape is
   *  added — which per-form wording and a per-basis vertex solver could not. See
   *  `readResponse`. */
  profile?: [number, number][];
  /** Whether the shape came from the YAML or was measured from history. `form:` is
   *  an override, not a prerequisite — omit it and the engine picks. */
  form_source?: "declared" | "inferred";
  /** Every shape considered, with a score comparable ACROSS shapes (AIC in
   *  y-space, lower is better). Empty when the form was declared.
   *
   *  Worth surfacing: it turns an inferred shape from a mystery into an argument —
   *  "a curve beat a line by 945" is checkable, "the engine chose quadratic" is
   *  not. `all_terms_significant` false means the candidate was never eligible,
   *  however good its score. */
  candidates?: { form: DriverForm; aic: number; all_terms_significant: boolean }[];
  refusal?: string;
}

export interface BaselineResponse {
  values: MeasureValues;
  unvalued: UnvaluedNode[];
  resolved_period: [string, string];
  /** Why the baseline produced no values, in words worth showing. Absent when
   *  measures were valued normally. The server distinguishes an executor
   *  error from an empty window from a column mismatch — three problems with
   *  three different fixes. */
  baseline_note?: string | null;
  /** Coefficients fitted for driver edges that declare none, plus refusals.
   *  Absent when every reachable driver edge already declares one. Echo these
   *  back into `predict` — it is database-free by design and cannot re-measure
   *  them on each keystroke. */
  fitted?: FittedDriver[];
}

// ── projection ──────────────────────────────────────────────────────────────

export type ProjectionGranularity = "day" | "week" | "month";

export interface ProjectionRequest {
  roots: string[];
  time_dimension: string;
  /** `[start, end]` inclusive date strings for the HISTORY — deliberately its
   *  own window, not the baseline's: a seasonal fit needs eight cycles, which
   *  is usually far more history than a scenario baseline averages over. */
  period: [string, string];
  instance?: BaselineInstance | null;
  granularity?: ProjectionGranularity;
  /** Buckets to project past the last historical one. */
  horizon: number;
  /** Seasonal periods, in buckets, applied to every measure in the request.
   *
   *  Omit it — that is not "use the default", it is "resolve per measure from
   *  whatever monitor already watches that series", which is what keeps the
   *  band here the band an anomaly had to breach. Send it only to pin a cycle
   *  no monitor has declared. Each period must be >= 2; `[]` is a 400. */
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
  /** The prediction interval. `null`/absent means the model returned no band —
   *  unknown spread, NOT a band of zero width. Never collapse these onto
   *  `point`. */
  lower?: number | null;
  upper?: number | null;
}

/** One measure's baseline curve: what happened, then what comes next.
 *
 *  An empty `forecast` with a `refusal` is a state, not a gap — most often
 *  "too little history to fit". It must never render as a flat forward line,
 *  which is what any code that defaults the missing curve to "unchanged"
 *  would draw. */
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
  resolved_period: [string, string];
  horizon: number;
  series: MeasureProjection[];
  /** Why the whole projection is empty, when it is. Absent when at least one
   *  measure produced history. */
  projection_note?: string | null;
}

export interface PredictOptions {
  /** Supplying values lets multiplicative edges be sized instead of
   *  returned `unquantifiable`. */
  values?: MeasureValues;
  /** The baseline's `fitted` array, verbatim. */
  coefficients?: FittedDriver[];
}

// ── explain ─────────────────────────────────────────────────────────────────

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
  /** Accumulated dimension-split filters; opaque to the UI. */
  filters: unknown[];
  delta: number;
  concentration: number;
  root_fraction: number;
  siblings?: ExplainSibling[];
  dimension_count?: number;
  children?: ExplainNode[];
}

/** Whether a driver's observed move pushes the target the way it actually
 *  moved, or against it. Mirrors `airlayer::…::DriverContribution`.
 *
 *  `counteracting` means the driver *offset* part of the move rather than
 *  causing it — e.g. discounts falling during a net-sales drop, where a
 *  `direction: negative` relationship means the fall pushed net sales up.
 *  `unknown` = no signed claim available (`direction: unknown` with no
 *  coefficient, or a flat driver/target). */
export type DriverContribution = "contributing" | "counteracting" | "unknown";

/** A driver's move split into the part its base forced and the part its own
 *  ratio contributed. Mirrors `airlayer::…::PassthroughSplit`.
 *
 *  Presence *is* the claim: only emitted when the driver genuinely tracks a
 *  sibling rather than moving on its own — discount *dollars* fall when volume
 *  falls, with no decision behind it. A pair whose ratio swings freely reports
 *  nothing at all, so there is no flag to check.
 *  `base_driven_delta + ratio_driven_delta === driver_delta`. */
export interface PassthroughSplit {
  base_measure: string;
  ratio_previous: number;
  ratio_current: number;
  /** The mechanical part — carries no information about the target. */
  base_driven_delta: number;
  /** The part with a decision behind it; routinely points the opposite way to
   *  the driver's raw delta. */
  ratio_driven_delta: number;
}

export interface DriverAttribution {
  driver_measure: string;
  driver_previous: number;
  driver_current: number;
  driver_delta: number;
  /** Declared direction of the relationship — needed to know which way
   *  `driver_delta` pushes the target.
   *
   *  Optional because `explain_cache` rows are returned verbatim with no schema
   *  version: an explain cached before these two fields shipped still
   *  deserializes here. Treat absent as unclassified, not as a default. */
  direction?: DriverDirection;
  contribution?: DriverContribution;
  coefficient?: number;
  form: DriverForm;
  /** Absent when there is no magnitude to report: a purely qualitative driver
   *  (no coefficient), or a non-linear `form` whose levels were unusable here
   *  (a zero denominator). Either way the sign of its push is carried by
   *  `contribution`, not by a magnitude — never read absence as zero. */
  estimated_target_impact?: number;
  description?: string;
  /** Set when this driver mechanically tracks a sibling driver rather than
   *  moving independently. Absent on pre-classification cached explains. */
  passthrough?: PassthroughSplit;
}

/** Mirrors `airlayer::engine::metric_tree_ops::ExplainWarning` —
 *  internally-tagged enum, `type` selects the variant. */
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

// ── opportunity ─────────────────────────────────────────────────────────────

/** How a segment's benchmark is computed from its peers. Serde
 *  `rename_all = "snake_case"`: `"median"` | `"p75"` | `"best_peer"`. */
export type BenchmarkStatistic = "median" | "p75" | "best_peer";

export interface SegmentOpportunity {
  segment: string;
  /** Benchmarked figure: a per-unit RATE (value / rows) when `weight_basis` is
   *  "rows", otherwise the raw measure value. */
  current_value: number;
  /** Row count in "rows" mode, a value share otherwise. */
  volume: number;
  /** Benchmark being compared against — a rate in "rows" mode. */
  benchmark: number;
  /** Gap to benchmark, same units as `current_value`. */
  gap: number;
  /** Addressable upside in measure units. In "rows" mode this is the rate gap
   *  applied to the segment's own volume, `(benchmark_rate − rate) × count`. */
  upside: number;
}

export interface DimensionOpportunity {
  dimension: string;
  cardinality: number;
  /** "best_peer" or "p75". */
  benchmark_basis: string;
  /** Summed over the significant segments, and summed BEFORE the tail trim and
   *  top-K cut — so it can exceed the sum of `segments`. */
  total_upside: number;
  segments: SegmentOpportunity[];
  /** Real segments left out of `segments`: those under 1% of the dimension's
   *  upside, plus any past the top-5 cap. The latter need not be small. */
  other_segments_skipped: number;
  /** Segments below the benchmark whose gap could not be distinguished from
   *  sampling noise, so were not sized. Distinct from the above: those were
   *  omitted for being minor, these for being unproven. */
  segments_dropped_as_noise: number;
}

export interface SkippedDimension {
  dimension: string;
  reason: string;
}

export interface OpportunityResult {
  target: string;
  period: [string, string];
  overall_value: number;
  /** How segments were weighted/compared: "rows" (sum-like sized on a per-unit
   *  rate with a `count` denominator), "value_share" (avg/min/max), or "equal"
   *  (ratios). Drives how to read `current_value`/`benchmark`/`gap`/`upside`. */
  weight_basis: string;
  dimensions: DimensionOpportunity[];
  skipped_dimensions: SkippedDimension[];
  downstream: PredictImpact[];
  /** `view.measure` id of the `count` measure the target was divided by to form
   *  the per-unit rates in `current_value`/`benchmark`. Present only in "rows"
   *  mode — the only mode that forms rates. Added by Oxy's handler, not
   *  airlayer: without it a rate is an unlabelled number. */
  rate_denominator?: string | null;
}

// ── drill ───────────────────────────────────────────────────────────────────

/** Mirrors `airlayer::engine::metric_tree_ops::CandidateKind` — externally
 *  tagged (no serde attrs): the variant name is the sole object key. */
export type CandidateKind =
  | { Component: { measure: string } }
  | { Dimension: { dimension: string; value: string } };

/** Mirrors `airlayer::engine::metric_tree_ops::StopReason` — a unit-variant
 *  enum, so serde emits it as a bare string. */
export type StopReason = "GateFailed" | "GateInconclusive" | "NoCandidates" | "MaxDepth";

/** A minimal mirror of airlayer's `QueryFilter` — the panel only needs
 *  `member` + `values` off of it. */
export interface DrillFilter {
  member?: string;
  values: string[];
}

export interface DrillCandidate {
  kind: CandidateKind;
  /** Share of THIS level's gap. */
  concentration: number;
  gap: number;
  gated: boolean;
}

export interface DrillLevel {
  measure: string;
  /** Accumulated numerator filters down to this level. */
  segment_filter: DrillFilter[];
  gap: number;
  /** Cascaded fraction of the ROOT gap. */
  root_share: number;
  /** Ranked; `[0]` is the one recursed into unless `stop_reason` is set. */
  candidates: DrillCandidate[];
  stop_reason: StopReason | null;
}

export interface DrillResult {
  target: string;
  root_gap: number;
  root_upside: number;
  benchmark_filter: DrillFilter[];
  levels: DrillLevel[];
}

/** Response of `POST /semantic/metric-tree/drill`. Mirrors
 *  `DrillResponse { #[serde(flatten)] result: Option<DrillResult>, rate_denominator }`:
 *  a `Some` result flattens `DrillResult`'s fields to the top level; a `None`
 *  omits them entirely (no `levels`). Model all `DrillResult` fields as
 *  optional here and treat a missing `levels` as "nothing to drill." */
export interface DrillResponse {
  target?: string;
  root_gap?: number;
  root_upside?: number;
  benchmark_filter?: DrillFilter[];
  levels?: DrillLevel[];
  /** Same role as `OpportunityResult.rate_denominator`: the `count` measure
   *  id the target was divided by to form rates, when applicable. */
  rate_denominator?: string | null;
}

/** Mirrors `airlayer::engine::metric_tree_ops::DrillRoot`. Names WHICH ranked
 *  row to decompose; the engine still derives that row's benchmark, gap and
 *  upside from its own scan. */
export interface DrillRoot {
  dimension: string;
  segment: string;
}

/** Request payload for `POST /semantic/metric-tree/drill`. */
export interface DrillRequest {
  target: string;
  time_dimension: string;
  period: [string, string];
  /** Narrow the scan to one instance. Omit to size the whole population. */
  instance?: OpportunityInstance;
  /** Optional override; defaults to airlayer's `DrillConfig::default()` (max_depth 5). */
  max_depth?: number;
  /** Optional override of the single-scan significance budget. */
  alpha?: number;
  /** Decompose this ranked row instead of the engine's top pick. Omit for the
   *  top pick. A row that is no longer in the scan comes back with no `levels`. */
  root?: DrillRoot;
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

// ── request payloads ────────────────────────────────────────────────────────

export interface PredictChange {
  measure: string;
  delta: number;
}

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

/** A world-model instance to scope a scan to, addressed as the world-model API
 *  addresses one: entity name plus instance key (a JSON array for a composite
 *  key, else a bare scalar). */
export interface OpportunityInstance {
  entity: string;
  key: string;
}

export interface OpportunityRequest {
  target: string;
  time_dimension: string;
  period: [string, string];
  /** Narrow the scan to one instance. Omit to size the whole population. */
  instance?: OpportunityInstance;
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

/** Response of `GET /semantic/metric-tree/time-dimensions`. */
export interface TimeDimensionsResponse {
  /** view name → fully-qualified time-dim ids (`view.dim`). */
  by_view: Record<string, string[]>;
}

/** Single-period distribution request. The baseline is auto-derived
 *  server-side as the equal-length window immediately before `period`. */
export interface DistributionRequest {
  target: string;
  time_dimension: string;
  period: [string, string];
}
