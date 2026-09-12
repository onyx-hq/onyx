// {{APP_DISPLAY_NAME}} — scaffolded by `create-oxy-app --template functions`.
//
// A standard Oxy custom app, plus a server-side **Oxy Function**. Oxy serves
// this bundle at `/customer-apps/<org>/{{APP_SLUG}}/`; `@oxy-hq/sdk` talks to
// your project from it. Three surfaces are wired up below:
//
//   1. `useQuery`         — raw SQL against the project's warehouse
//   2. `useSemanticQuery` — measures + dimensions from a semantic topic
//   3. `useFunction`      — invoke `functions/notify.ts`, which runs on Oxy's
//                           isolate runtime and sends email
//
// Auth rides the session cookie: Oxy only serves this bundle after a
// membership check, so `/api/*` calls from here are already authenticated.

import { useFunction, useQuery, useSemanticQuery } from "@oxy-hq/sdk";
import { useState } from "react";
import { Button, fieldClass } from "./chrome/Button";
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
// dimension and one measure it exposes. Member paths are `topic.member`.
const TOPIC = "your_topic";
const DIMENSION = "your_dimension";
const MEASURE = "your_measure";

interface StarterRow {
  dataset: string;
  records: number;
}

interface NotifyResult {
  ok: boolean;
  messageId: string;
  sentTo: string;
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

function Note({ children }: { children: React.ReactNode }) {
  return <p className='text-[11px] text-muted-foreground leading-relaxed'>{children}</p>;
}

function ErrorBlock({ title, message }: { title: string; message: string }) {
  return (
    <div className='flex flex-col gap-1 border border-status-error/40 bg-status-error/5 p-2'>
      <div className='font-mono text-[11px] text-status-error'>{title}</div>
      <div className='max-h-28 overflow-auto whitespace-pre-wrap break-words font-mono text-[10px] text-muted-foreground'>
        {message}
      </div>
    </div>
  );
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

      <main className='relative flex min-h-0 flex-1 flex-wrap content-start gap-2.5 overflow-auto p-2.5'>
        <div className='basis-full flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 border border-border bg-card px-3 py-2'>
          <div className='text-[11px] text-muted-foreground'>
            A standard Oxy custom app with a server-side function. Edit{" "}
            <code>src/App.tsx</code> and <code>functions/notify.ts</code> to make it yours.
          </div>
          <div className='font-mono text-[10px] text-muted-foreground uppercase tracking-wider'>
            useQuery · useSemanticQuery · useFunction
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
        <NotifyCard />

        <Panel title='Next steps' className='basis-full'>
          <Note>
            Replace <code>STARTER_SQL</code> with a query against your warehouse, point{" "}
            <code>TOPIC</code> at one of your semantic topics, edit the email in{" "}
            <code>emails/Welcome.tsx</code> (preview it with <code>pnpm email:dev</code>), then
            ship it all with <code>oxyc publish</code>.
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
        <ErrorBlock title='Query failed' message={query.error.message} />
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
        These rows are literals so the scaffold renders with no tables set up — swap{" "}
        <code>STARTER_SQL</code> for a real query.
      </Note>
    </Panel>
  );
}

// ── useSemanticQuery · the semantic model ───────────────────────────────────

function SemanticPanel() {
  const semantic = useSemanticQuery<Record<string, unknown>>({
    topic: TOPIC,
    dimensions: [`${TOPIC}.${DIMENSION}`],
    measures: [`${TOPIC}.${MEASURE}`],
    limit: 5
  });

  const columns = semantic.rows.length > 0 ? Object.keys(semantic.rows[0]) : [];
  // Until TOPIC points at a real topic this errors — expected on a fresh
  // scaffold, so present it as an instruction rather than a failure.
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
          file) and this panel fills in.
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

// ── useFunction · a server-side handler ─────────────────────────────────────
//
// `useFunction(name)` returns an imperative `invoke(body?)` that POSTs to the
// function's route. The handler in functions/notify.ts runs on Oxy's isolate
// with a data-plane `ctx` — it can reach your warehouse, secrets, storage and
// email without any of that touching the browser.

function NotifyCard() {
  const fn = useFunction<NotifyResult>("notify");
  const [name, setName] = useState("");

  return (
    <Panel
      title='Server-side function · useFunction'
      right={<span className='font-mono text-[10px] normal-case'>notify</span>}
      className='grow basis-[24rem]'
    >
      <Note>
        Runs <code>functions/notify.ts</code> on Oxy's isolate runtime, which renders{" "}
        <code>emails/Welcome.tsx</code> and sends it with <code>ctx.email.send</code>. The
        recipient is <em>you</em> — derived server-side from <code>ctx.user.email</code>, never
        from the browser.
      </Note>

      <form
        className='flex flex-wrap items-center gap-2'
        onSubmit={(e) => {
          e.preventDefault();
          // invoke() rejects on error; the hook also exposes fn.error, so
          // swallow the rejection to avoid an unhandled promise.
          if (!fn.isLoading) fn.invoke({ name: name.trim() }).catch(() => {});
        }}
      >
        <label
          htmlFor='notify-name'
          className='font-mono text-[10px] text-muted-foreground uppercase tracking-wider'
        >
          Name
        </label>
        <input
          id='notify-name'
          type='text'
          placeholder='Ada'
          value={name}
          onChange={(e) => setName(e.target.value)}
          className={`${fieldClass} min-w-[8rem] flex-1`}
        />
        <Button variant='primary' type='submit' disabled={fn.isLoading}>
          {fn.isLoading ? "Sending…" : "Email me"}
        </Button>
      </form>

      {fn.error && (
        <ErrorBlock title='Function failed' message={fn.error.message.split("\n")[0]} />
      )}

      {fn.data && !fn.error && (
        <div className='flex items-center gap-2 border border-status-success/40 bg-status-success/10 px-2.5 py-1.5 font-mono text-[11px] text-foreground'>
          <span className='size-1.5 shrink-0 rounded-full bg-status-success' />
          <span className='flex-1 truncate'>Sent to {fn.data.sentTo}</span>
        </div>
      )}

      <Note>
        A real send needs a verified SES sender. On a dev server set{" "}
        <code>OXY_APP_EMAIL_LOCAL_TEST=1</code> to preview in the browser instead, or run{" "}
        <code>pnpm email:dev</code> to render the template on its own.
      </Note>
    </Panel>
  );
}
