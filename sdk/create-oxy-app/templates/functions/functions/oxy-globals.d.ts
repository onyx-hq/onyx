// Globals the Oxy Functions isolate provides. Declared here rather than in
// `@oxy-hq/sdk` on purpose: this file is picked up ONLY by functions/tsconfig.json
// (which has no DOM lib), so these types can never leak into the browser half of
// the app, where `btoa` exists but behaves differently.
//
// Not a function entry — `oxyc publish` bundles the files named in oxy-app.json's
// `functions` map, so a .d.ts sitting here is inert.

/**
 * Base64-encode a **Latin1 string**.
 *
 * Characters above U+00FF throw, and U+0080..U+00FF encode as one byte each —
 * meaning `btoa(utf8Text)` yields mojibake, not an error. For text, don't
 * encode at all: pass the string with `{ encoding: "utf8" }` to
 * `ctx.email.send` attachments or `ctx.storage.put`.
 *
 * For **bytes**, use `bytesToBase64` from `@oxy-hq/sdk`. Passing a
 * `Uint8Array` here throws: the Web spec would stringify it to "37,80,68,70"
 * and silently encode that text instead of your file.
 */
declare function btoa(data: string): string;

/**
 * Decode base64 to a Latin1 string. Throws on malformed input (including a
 * misplaced `=`) rather than returning a truncated result.
 *
 * To get bytes back, use `base64ToBytes` from `@oxy-hq/sdk`.
 */
declare function atob(data: string): string;
