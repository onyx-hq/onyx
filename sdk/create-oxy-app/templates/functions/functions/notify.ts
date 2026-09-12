// notify — an example Oxy Function: a server-side handler that emails the
// INVOKING USER a welcome message. Oxy bundles `functions/*` at `oxyc publish`
// and runs them in a sandboxed isolate with a data-plane `ctx`.
//
// It's declared in oxy-app.json under `functions.notify` with
// `"email": { "send": true }` — the fail-closed capability that lets it call
// `ctx.email.send`. Without that capability the host rejects the send.
//
// SECURITY — the recipient is derived server-side (`ctx.user.email`), NEVER
// taken from the request body. Mail goes from the platform's shared verified
// sender, so a caller-supplied `to` is an open relay / spam cannon (and burns
// SES reputation for everyone). Always derive recipients server-side — the
// invoking user, or an address looked up from your own data. Template DATA
// (like the display name below) can come from the request; the recipient
// must not.
//
// Invoke it from the frontend (no recipient — it goes to you):
//   const { invoke } = useFunction("notify");
//   await invoke({ name: "Ada" });
// or add a `schedule` in oxy-app.json to run it in the background.
//
// LOCAL DEV: on a cloud-mode dev server, set OXY_APP_EMAIL_LOCAL_TEST=1 to
// preview the rendered email in the browser instead of sending it via SES.
// To iterate on the template alone, `pnpm email:dev` renders it with sample
// props — no server needed.

import type { OxyFunctionContext, OxyFunctionRequest } from "@oxy-hq/sdk";
import { render } from "@oxy-hq/sdk/email";
import { Welcome } from "../emails/Welcome";

interface NotifyBody {
  name?: string;
}

/** A tiny CSV, built in the handler — stands in for a real generated report. */
function buildChecklistCsv(): string {
  const rows = [
    ["step", "what to do"],
    ["1", "Connect a warehouse"],
    ["2", "Ask a question in chat"],
    ["3", "Publish your first app"]
  ];
  return rows.map((cols) => cols.join(",")).join("\n");
}

export default async function notify(
  req: OxyFunctionRequest,
  ctx: OxyFunctionContext
): Promise<Response> {
  const { name } = JSON.parse(req.body || "{}") as NotifyBody;

  // `ctx.user.email` is `string | null`: a frontline worker — enrolled by PIN
  // on a shared device — has no mailbox, and the platform stores none rather
  // than inventing one. Say so, rather than handing the sender a null.
  const to = ctx.user.email;
  if (to === null) {
    return Response.json(
      { error: "the caller has no email address, so there is nobody to send to" },
      { status: 400 }
    );
  }

  const { messageId } = await ctx.email.send({
    to,
    subject: "Welcome!",
    html: render(Welcome, { name: name || to }),
    // ATTACHMENTS — `encoding` says how to read `content`, and the right value
    // depends on where the bytes came from:
    //
    //   generated text (this CSV)  encoding: "utf8" — no encoder needed, and
    //                              byte-exact for accented characters
    //   a stored asset             ctx.storage.get(key, { encoding: "base64" })
    //   a remote file              ctx.fetch(url, { encoding: "base64" })
    //   bytes you built yourself   bytesToBase64(u8) from @oxy-hq/sdk
    //
    // Reach for "utf8" whenever the content is text. `btoa` reads strings as
    // Latin1, so btoa(csvWithAccents) silently produces mojibake rather than
    // an error — it is for BYTES, not for text.
    //
    // Caps: 20 attachments and 10 MiB decoded per send. For anything bigger,
    // put the file in ctx.storage and email a presigned link instead.
    attachments: [
      {
        filename: "getting-started.csv",
        content: buildChecklistCsv(),
        encoding: "utf8",
        contentType: "text/csv"
      }
    ]
  });

  return Response.json({ ok: true, messageId, sentTo: ctx.user.email });
}
