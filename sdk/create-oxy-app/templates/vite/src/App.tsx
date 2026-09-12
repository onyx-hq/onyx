// {{APP_DISPLAY_NAME}} — scaffolded by `create-oxy-app`.
//
// A standard Oxy custom app: a Vite + React bundle that Oxy serves at
// `/customer-apps/<org>/{{APP_SLUG}}/` and that talks to your project through
// `@oxy-hq/sdk`. Two data surfaces are wired up below:
//
//   1. `useQuery`         — raw SQL against the project's warehouse
//   2. `useSemanticQuery` — measures + dimensions from a semantic topic
//
// Auth rides the session cookie: Oxy only serves this bundle after a
// membership check, so `/api/*` calls from here are already authenticated —
// there is no token to manage in the frontend.
//
// To make it yours: swap `STARTER_SQL` for a real query, and point `TOPIC` at
// one of your project's `.topic.yml` topics.

import { useQuery, useSemanticQuery } from "@oxy-hq/sdk";
import { KpiTile } from "./chrome/KpiTile";
import { Panel } from "./chrome/Panel";
import { Pill } from "./chrome/Pill";
import { Topbar } from "./chrome/Topbar";

const APP_NAME = "{{APP_DISPLAY_NAME}}";
const APP_SLUG = "{{APP_SLUG}}";

// ── Replace these with your own ─────────────────────────────────────────────

// A literal SELECT needs no tables, so a freshly scaffolded app renders real
// results on first run with nothing set up. Point it at your warehouse when
// you're ready, e.g.:
//
//   SELECT status AS dataset, COUNT(*) AS records FROM orders GROUP BY status
const STARTER_SQL = `
  SELECT 'orders' AS dataset, 1284 AS records
  UNION ALL SELECT 'customers', 317
  UNION ALL SELECT 'shipments', 902
`;

// A topic from your project's semantic model (a `.topic.yml` file), plus one
// dimension and one measure it exposes. Member paths are `topic.member`. The
// panel below explains itself until these point at something real.
const TOPIC = "your_topic";
const DIMENSION = "your_dimension";
const MEASURE = "your_measure";

interface StarterRow {
  dataset: string;
  records: number;
}

const fmtInt = (n: number) => n.toLocaleString();

// ── Shared bits ─────────────────────────────────────────────────────────────

const HEAD_ROW =
  "border-border border-b text-[10px] text-muted-foreground uppercase tracking-wider";
const BODY_ROW = "border-border/40 border-b text-foreground last:border-b-0";

function Loading({ label = "Loading…" }: { label?: string }) {
  return (
    <div className='flex items-center gap-2 py-1.5 font-mono text-[11px] text-muted-foreground'>
      <span className='size-1.5 animate-pulse rounded-full bg-muted-foreground' />
      {label}
    </div>
  );
}

// A muted explanatory footnote — the "how do I change this" prose lives here.
function Note({ children }: { children: React.ReactNode }) {
  return <p className='text-[11px] text-muted-foreground leading-relaxed'>{children}</p>;
}

export function App() {
  const starter = useQuery<StarterRow>({ sql: STARTER_SQL });

  const totalRecords = starter.rows.reduce((sum, r) => sum + Number(r.records ?? 0), 0);
  const status = starter.error ? "error" : starter.loading ? "connecting" : "connected";

  return (
    <div className='flex h-full w-full flex-col'>
      <Topbar
        breadcrumb={
          <>
            {APP_SLUG} · <b className='font-medium text-foreground'>{APP_NAME}</b>
          </>
        }
      />

      {/* Flex-wrap rather than a grid: each card is `grow basis-[24rem]`, so
          cards reflow to 3/2/1 per row by width AND grow to fill whatever row
          they land on — a lone card stretches full width instead of leaving a
          gap. */}
      <main className='relative flex min-h-0 flex-1 flex-wrap content-start gap-2.5 overflow-auto p-2.5'>
        <div className='basis-full flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 border border-border bg-card px-3 py-2'>
          <div className='text-[11px] text-muted-foreground'>
            A standard Oxy custom app. Edit <code>src/App.tsx</code> to make it yours.
          </div>
          <div className='font-mono text-[10px] text-muted-foreground uppercase tracking-wider'>
            useQuery · useSemanticQuery
          </div>
        </div>

        <div className='basis-full grid grid-cols-2 gap-2.5 md:grid-cols-3'>
          <KpiTile
            label='Rows returned'
            value={fmtInt(starter.rows.length)}
            loading={starter.loading}
          />
          <KpiTile label='Total records' value={fmtInt(totalRecords)} loading={starter.loading} />
          <KpiTile label='Project connection' value={status} />
        </div>

        <StarterQueryPanel query={starter} />
        <SemanticPanel />

        <Panel title='Next steps' className='basis-full'>
          <Note>
            Replace <code>STARTER_SQL</code> with a query against your warehouse, point{" "}
            <code>TOPIC</code> at one of your semantic topics, then ship it with{" "}
            <code>oxyc publish</code>. Re-skin everything by editing the tokens in{" "}
            <code>src/index.css</code>.
          </Note>
        </Panel>
      </main>
    </div>
  );
}

// ── useQuery · raw SQL ──────────────────────────────────────────────────────

function StarterQueryPanel({ query }: { query: ReturnType<typeof useQuery<StarterRow>> }) {
  return (
    <Panel
      title='Warehouse query · useQuery'
      right={<span className='font-mono text-[10px] normal-case'>raw SQL</span>}
      className='grow basis-[24rem]'
    >
      {query.loading ? (
        <Loading />
      ) : query.error ? (
        <div className='flex flex-col gap-1 border border-status-error/40 bg-status-error/5 p-2'>
          <div className='font-mono text-[11px] text-status-error'>Query failed</div>
          <div className='max-h-28 overflow-auto whitespace-pre-wrap break-words font-mono text-[10px] text-muted-foreground'>
            {query.error.message}
          </div>
        </div>
      ) : (
        <table className='w-full border-collapse font-mono text-[11px]'>
          <thead>
            <tr className={HEAD_ROW}>
              <th className='py-1 text-left font-normal'>Dataset</th>
              <th className='py-1 text-right font-normal'>Records</th>
            </tr>
          </thead>
          <tbody>
            {query.rows.map((row) => (
              <tr key={row.dataset} className={BODY_ROW}>
                <td className='py-1 text-left'>{row.dataset}</td>
                <td className='py-1 text-right'>{fmtInt(Number(row.records))}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <Note>
        The SQL runs against your project's warehouse and is defined in{" "}
        <code>src/App.tsx</code>. These rows are literals so the scaffold renders with no tables
        set up — swap <code>STARTER_SQL</code> for a real query.
      </Note>
    </Panel>
  );
}

// ── useSemanticQuery · the semantic model ───────────────────────────────────
//
// The same shape as useQuery, but the measures come from your `.view.yml` /
// `.topic.yml` files instead of SQL in this file. When the data team refactors
// the SQL behind a measure, this panel follows with no edit here.

function SemanticPanel() {
  const semantic = useSemanticQuery<Record<string, unknown>>({
    topic: TOPIC,
    dimensions: [`${TOPIC}.${DIMENSION}`],
    measures: [`${TOPIC}.${MEASURE}`],
    limit: 5
  });

  const columns = semantic.rows.length > 0 ? Object.keys(semantic.rows[0]) : [];
  // Until TOPIC points at a real topic this errors — that is expected on a
  // fresh scaffold, so present it as an instruction rather than a failure.
  const unconfigured = Boolean(semantic.error) || semantic.rows.length === 0;

  return (
    <Panel
      title='Semantic model · useSemanticQuery'
      right={
        <span className='font-mono text-[10px] normal-case'>
          {unconfigured ? <Pill variant='warn'>NOT SET UP</Pill> : `topic: ${TOPIC}`}
        </span>
      }
      className='grow basis-[24rem]'
    >
      {semantic.loading ? (
        <Loading />
      ) : unconfigured ? (
        <Note>
          Point <code>TOPIC</code>, <code>DIMENSION</code> and <code>MEASURE</code> in{" "}
          <code>src/App.tsx</code> at a topic your project defines (a <code>.topic.yml</code>{" "}
          file) and this panel fills in. Member paths are <code>topic.member</code>.
        </Note>
      ) : (
        <table className='w-full border-collapse font-mono text-[11px]'>
          <thead>
            <tr className={HEAD_ROW}>
              {columns.map((c) => (
                <th key={c} className='py-1 text-left font-normal'>
                  {c}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {semantic.rows.map((row, i) => (
              // Semantic rows have no natural id; index is stable for a
              // rendered result set.
              // biome-ignore lint/suspicious/noArrayIndexKey: no stable row id
              <tr key={i} className={BODY_ROW}>
                {columns.map((c) => (
                  <td key={c} className='py-1 text-left'>
                    {String(row[c])}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}
