#!/usr/bin/env python3
"""Writes .oxy-dev/state.json for scripts/dev-up.sh and prints its closing summary.

Invoked by dev-up.sh with its values in the environment. Everything recorded is
observed rather than assumed: personas come from asking the running server's
dev-login endpoint, workspaces and custom apps from the database it serves. A
persona the seed never created therefore reads as refused here, not as a name.
Tokens are never written to disk.
"""

import json
import os
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone

ENV = os.environ.get
BACKEND = ENV("BACKEND_URL", "http://127.0.0.1:3000")
FRONTEND = ENV("FRONTEND_URL", "http://127.0.0.1:5173")
PERSONAS = ("staff", "owner", "member", "operator", "partner")
# The dev-login persona roster, reported only when a probe cannot say who a persona is.
EXPECTED = {
    "staff": (ENV("STAFF_EMAILS") or "").split(",")[0].strip() or None,
    "owner": "maya.nguyen@acme.test",
    "member": "amara.larsson@acme.test",
    "operator": "app-operator@oxy.local",
    "partner": "oliver.okafor@acme.test",
}
# Uuid::new_v5(NAMESPACE_DNS, "demo.oxy.local") — crates/app/src/cli/commands/seed.rs.
DEMO_WORKSPACE = "70787bb2-e11b-5488-b2c3-02e60d5fc7d3"
# Loopback must never be routed through an HTTP(S)_PROXY from the caller's environment.
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

WORKSPACES_SQL = """
SELECT o.slug AS org_slug, w.name AS workspace_name, w.id::text AS workspace_id,
       w.status, (w.current_revision_id IS NOT NULL) AS compiled
FROM workspaces w LEFT JOIN organizations o ON o.id = w.org_id
ORDER BY o.slug NULLS LAST, w.name
"""
APPS_SQL = """
SELECT o.slug AS org_slug, a.slug AS app_slug, a.name, a.status
FROM apps a JOIN organizations o ON o.id = a.org_id
ORDER BY o.slug, a.slug
"""


def warn(message):
    print(f"warn: {message}", file=sys.stderr)


def probe(persona):
    """Who `?as=<persona>` signs in as, via the GET form (token in body, no cookie)."""
    url = f"{BACKEND}/api/auth/dev-login?as={persona}"
    try:
        with OPENER.open(url, timeout=10) as resp:
            body = json.load(resp)
    except urllib.error.HTTPError as err:
        detail = err.read(300).decode("utf-8", "replace").strip()
        return {"http": err.code, "error": detail}
    except (OSError, ValueError) as err:
        return {"http": None, "error": str(err)}
    user = body.get("user") or {}
    return {
        "http": 200,
        "email": user.get("email"),
        "is_owner": user.get("is_owner"),
        "is_app_admin": user.get("is_app_admin"),
        "orgs": [f"{o.get('slug')}:{o.get('role')}" for o in body.get("orgs") or []],
    }


def psql_rows(sql):
    """Rows as dicts, or None when the database cannot be read."""
    query = f"SELECT coalesce(json_agg(t), '[]'::json) FROM ({sql}) t"
    cmd = [
        "docker",
        "exec",
        "oxy-postgres",
        "psql",
        "-U",
        "postgres",
        "-d",
        "oxy",
        "-tAc",
        query,
    ]
    try:
        out = subprocess.run(
            cmd, capture_output=True, text=True, timeout=20, check=True
        )
        return json.loads(out.stdout.strip() or "[]")
    except (OSError, subprocess.SubprocessError, ValueError) as err:
        lines = (getattr(err, "stderr", None) or str(err)).strip().splitlines()
        last = lines[-1][:200] if lines else ""
        warn(f"could not read the database through `docker exec oxy-postgres`: {last}")
        return None


def as_int(value):
    return int(value) if value and value.isdigit() else None


def refusal(result):
    code = result["http"]
    if code == 409:
        return "NOT SEEDED - run `just up` (without --no-seed)"
    if code == 400:
        return "unknown persona - this binary predates it (rebuild?)"
    if code == 404:
        return "dev-login disabled (no OXY_GLOBAL_ADMINS in .env, or a release binary)"
    if code in (401, 403):
        return f"refused ({code}; OXY_DEV_LOGIN_EMAILS set without this identity?)"
    return f"unavailable ({code}: {(result.get('error') or '')[:80]})"


def summarize(state, probes):
    running = state["frontend_running"]
    print(f"    frontend  {FRONTEND}" + ("" if running else "  (not running)"))
    print(f"    backend   {BACKEND}   internal, no auth: {state['internal_url']}")
    print(f"    database  {state['database_url']}")
    print(f"    state     {os.path.relpath(ENV('STATE_JSON'), ENV('REPO'))}")
    print("\n    personas  (GET /api/auth/dev-login?as=<persona>)")
    for persona, result in probes.items():
        if result["http"] == 200:
            line = f"{result['email']}  orgs: {', '.join(result['orgs']) or '-'}"
        else:
            line = refusal(result)
        print(f"      {persona:<9} {line}")

    signed_in = [r["email"] for r in probes.values() if r["http"] == 200]
    if len(signed_in) > 1 and len(set(signed_in)) == 1:
        warn(
            "every persona signed in as the same user - this binary ignores ?as= (rebuild?)"
        )
    demo = next(
        (w for w in state["workspaces"] if w["workspace_id"] == DEMO_WORKSPACE), None
    )
    if demo is None and state["database_readable"]:
        warn(
            "the Demo workspace is not in the database - seed has not run (drop --no-seed)"
        )
    elif demo is not None and not demo["compiled"]:
        warn(
            "the Demo workspace has no promoted revision - its compiled reads will 503"
        )

    staff_next = f"/local/workspaces/{DEMO_WORKSPACE}/ide" if demo else "/admin"
    links = (("member", "/"), ("owner", "/acme"), ("staff", staff_next))
    if running:
        print("\n    open signed in")
        for persona, path in links:
            nxt = urllib.parse.quote(path, safe="/")
            print(f"      {persona:<7} {FRONTEND}/dev-login?as={persona}&next={nxt}")
    else:
        print(f"\n    token: curl '{BACKEND}/api/auth/dev-login?as=member'")


def main():
    probes = {p: probe(p) for p in PERSONAS}
    workspaces = psql_rows(WORKSPACES_SQL)
    readable = workspaces is not None
    workspaces = workspaces or []
    for ws in workspaces:
        if ws.get("org_slug"):
            ws["home_path"] = f"/{ws['org_slug']}/workspaces/{ws['workspace_id']}/home"
    apps = (psql_rows(APPS_SQL) or []) if readable else []
    for app in apps:
        app["url"] = f"{FRONTEND}/customer-apps/{app['org_slug']}/{app['app_slug']}/"

    state = {
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "repo": ENV("REPO"),
        "frontend_url": FRONTEND,
        "frontend_running": bool(ENV("FRONTEND_PID")),
        "backend_url": BACKEND,
        "internal_url": ENV("INTERNAL_URL"),
        "database_url": ENV("DATABASE_URL"),
        "binary": ENV("BIN"),
        "pids": {
            "backend": as_int(ENV("BACKEND_PID")),
            "frontend": as_int(ENV("FRONTEND_PID")),
        },
        "logs": {
            k: ENV(f"{k.upper()}_LOG") or None
            for k in ("backend", "frontend", "seed", "build")
        },
        "dev_login_template": f"{FRONTEND}/dev-login?as={{persona}}&next={{path}}",
        "token_template": f"{BACKEND}/api/auth/dev-login?as={{persona}}",
        "personas": {
            p: (r["email"] if r["http"] == 200 else EXPECTED[p])
            for p, r in probes.items()
        },
        "persona_checks": probes,
        "database_readable": readable,
        "workspaces": workspaces,
        "custom_apps": apps,
    }
    path = ENV("STATE_JSON")
    with open(f"{path}.tmp", "w", encoding="utf-8") as out:
        json.dump(state, out, indent=2)
        out.write("\n")
    os.replace(f"{path}.tmp", path)
    summarize(state, probes)


if __name__ == "__main__":
    main()
