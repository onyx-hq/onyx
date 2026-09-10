# Changelog

All notable changes to this project will be documented in this file.

## [0.5.144] - 2026-09-10

### 🚀 Features

- Custom-app webhooks — the platform verifies the sender, the manifest declares the secret (#3137)
- Custom-app secrets — a write path, and a panel that says what is missing (#3150)

### 🐛 Bug Fixes

- Router health probe is a metric, not 67% of the log volume (#3149)
- Serve execute_sql's sql_file from the compile boundary (#3151)

### 💼 Other

- *(deps)* Bump softprops/action-gh-release from 3.0.2 to 3.0.3 (#3143)
- *(deps)* Bump lewagon/wait-on-check-action from 1.8.1 to 1.9.1 (#3144)

### ⚙️ Miscellaneous Tasks

- Local airhouse stack tracks image 0.1.48 (#3139)

## [0.5.143] - 2026-09-09

### 🚀 Features

- Custom apps sign out — links.login on the shell context, signOut() in the SDK (#3135)
- *(sp_api)* Bump airway to 0.1.41 and reach the Vendor Central reports (#3134)

### ⚙️ Miscellaneous Tasks

- Release 0.5.143 (#3136)

## [0.5.142] - 2026-09-08

### 🚜 Refactor

- Close the old-executor retirement loose ends (dead telemetry, runs sweep, renames, checkpoints drop) (#3123)

### ⚙️ Miscellaneous Tasks

- Release 0.5.142 (#3133)

## [0.5.141] - 2026-09-08

### 🐛 Bug Fixes

- Http spans — only the served probe paths are silent, a failing probe says so, one probe list (#3128)
- Workspace health reads the semantic model through the compile boundary (#3124)
- Workspace health — anomalies no longer decide the verdict (#3126)
- Logs — SDK and AWS targets always suppressed, per-tick lines to debug, the alerter warns once an hour per workspace (#3131)
- Logs — throttle state in AlertState, prefix-honest noisy list, product store gets the same suppressions (#3132)

### ⚙️ Miscellaneous Tasks

- Release 0.5.141 (#3130)

## [0.5.140] - 2026-09-08

### 🚀 Features

- Llm calls — OpenTelemetry GenAI semconv on every model-call span, content capture opt-in, tenant context (#3119)
- Custom-app functions — the host is the audit record for every OLTP and Airhouse write (#3121)
- Audit log — append-only at the database, chain heads anchored hourly to S3 Object Lock (#3122)

### 🐛 Bug Fixes

- Http spans — readiness, liveness and health probes get no span and no log line (#3127)

### 🚜 Refactor

- Drop the execute module, relocate live utils to exec_runtime (Phase 4c) (#3070)

### 📚 Documentation

- Custom-app function observability — the Workers direction, gaps and five slices (#3125)

### ⚙️ Miscellaneous Tasks

- Release 0.5.140 (#3120)

## [0.5.139] - 2026-09-07

### 🚀 Features

- The operating graph — places, positions, assignments, reach, and Place in the semantic model (slices one to three) (#3113)
- Org settings before a workspace exists — the crew first, and a settings deep link (#3114)
- Platform telemetry — OTLP traces + logs to ClickStack/HyperDX, flat JSON logs, semconv HTTP spans (#3115)
- Custom-app functions — one trace per invocation, from the browser click to the warehouse query (#3118)

### 📚 Documentation

- Custom-app function telemetry on the platform trace — design (#3117)

### ⚙️ Miscellaneous Tasks

- Release 0.5.139 (#3116)

## [0.5.138] - 2026-09-07

### 🚀 Features

- Custom-app function logs reach observability, and one request id (#3095)
- Derive custom-app availability from real traffic, and make logs durable (#3096)
- Open the prod image-bump PR at release time, and announce the deploy (#3098)
- The three platform gaps custom apps kept hitting — people, frontline, migrations (#3099)
- A frontline worker can be suspended and reinstated (#3100)
- The directory names whoever can reach the app, crew included (#3101)
- Authz — the custom-app data plane is a workspace, and the crew can invoke functions (#3102)
- A tracked event names the app it belongs to (#3104)
- Crew sign-in — a PIN works only on an enrolled kiosk, and the login page offers it (#3106)
- Crew section — a manager enrols workers, grants apps, resets PINs and enrols kiosks from Settings (#3111)
- Metric-tree scenario simulation (#2862)

### 🐛 Bug Fixes

- A frontline worker can be granted an app — from enrolment, or from the app's access settings (#3108)
- The enrol link is minted on the host the request arrived on (#3109)

### 💼 Other

- *(deps)* Update 25 npm dependencies to their latest minor/patch (#3088)
- Let sp_api accept a date-window backfill, and pin the release that can serve one (#3093)

### 🚜 Refactor

- Custom_apps_migrations is a directory — types, plan, apply (#3103)

### 📚 Documentation

- Comprehensive rewrite of `@oxy-hq/cli` README and maintainer guide (#3090)
- Comprehensive rewrite of `@oxy-hq/cli` README and maintainer guide (#3090)
- Refresh ownership matrix + CODEOWNERS (weekly) (#3112)

### 🧪 Testing

- Oxy-app tests — eleven top-level targets become one group and four folds (#3110)

### ⚙️ Miscellaneous Tasks

- Remove 2 draft docs
- @oxy-hq/sdk 2.11.0 (#3105)
- The SDK publish skips a version that is already on the registry (#3107)
- Release 0.5.138 (#3089)

## [0.5.137] - 2026-09-03

### 🐛 Bug Fixes

- *(observability)* Partition event tables by day, not month (#3085)

### ⚙️ Miscellaneous Tasks

- Release 0.5.137 (#3086)

## [0.5.136] - 2026-09-03

### 🐛 Bug Fixes

- Refuse `skills install` from a throwaway package cache, and smoke-test the compiled binary (#3082)

### ⚙️ Miscellaneous Tasks

- Release @oxy-hq/cli 0.2.0, and read `--version` from the manifest (#3084)
- Release 0.5.136 (#3083)

## [0.5.135] - 2026-09-03

### 🚀 Features

- Custom apps are installable — a synthesised web app manifest (#3051)
- Promote-after-migrate, and a user who has no email address (#3031)
- Frontline sign-in — a session that can name a user with no email (#3042)
- The assignment graph — locations, tenant roles, and work items (#3050)
- Notifications — the inbox, and the seam push plugs into (#3053)
- Chat — channels, messages, and cross-replica delivery (#3048)
- Chat channels can be created and joined (#3066)
- A real Web Push sender behind the Push seam (#3068)
- Interactive airway pipelines execute on the worker fleet, not the pod that accepted them (#3056)
- Oxyc — a TypeScript CLI replacing `oxy api`, with served route discovery (#3016)
- Ship oxyc as a self-contained binary with a one-line installer
- Serve the VAPID public key, so a browser can actually subscribe (#3079)

### 🐛 Bug Fixes

- Custom-app asset manifest doubled the mount prefix in every entry (#3054)
- A custom-app function could never write an airhouse-managed database (#3052)
- Main did not compile, and the OLTP app-delete guard named an unreachable remedy (#3071)
- *(observability)* Stop the span-drop warning amplifying under store outage (#3076)
- Point sdk package repository URLs at the real repo
- *(observability)* Retention TTL has never applied — DateTime64 is rejected (#3077)
- A custom-app function's HTTP status now reaches its caller (#3080)
- Oxyc routes --json help, and Link: rel="next" for the lists that paged silently (#3081)

### 💼 Other

- *(deps)* Bump docker/setup-buildx-action from 4.2.0 to 4.3.0 (#3072)
- *(deps)* Bump github/codeql-action/upload-sarif from 4.37.7 to 4.37.9 (#3073)

### 🚜 Refactor

- Delete the Executable trait, its last two impls off it (retirement, Phase 4b) (#3064)

### 📚 Documentation

- Rename "semantic layer" to "semantic model" in copy and docs (#3063)
- Custom apps — who a function's database connection runs as (#3069)
- Biweekly internal-docs distillation (#3058)
- Document oxyc usage now that @oxy-hq/cli is published (#3078)

### 🧪 Testing

- Assert the assignment graph's cross-tenant gates (#3067)

### ⚙️ Miscellaneous Tasks

- Release 0.5.135 (#3065)

## [0.5.134] - 2026-09-02

### 🚀 Features

- *(airway)* Wire the SP-API source kind (#3044)
- *(pipelines)* Add Amazon Selling Partner to the New Pipeline wizard (#3046)
- Oxy Functions can send binary, see the request, and verify signatures (#3032)
- Connect any OAuth provider from a custom app, Google Drive first (#3055)

### 🐛 Bug Fixes

- Make the semantic engine cache the only door, keyed on the layer actually read (#3037)
- Retire orphaned runs and starved pollers that exhausted the Postgres pool (#3059)
- Key the semantic layer cache on the source it was read from (#3036)
- Retire path-less-workspace runs too, and make the transport contract say so (#3062)

### 🚜 Refactor

- Oxy-semantic drops 2,341 lines nothing calls (#3033)
- One crate owns the airlayer boundary, not nine (#3034)
- Delete the dead ExecutableBuilder pipeline, checkpoint storage & legacy run-dispatcher (retirement, Phase 4a) (#3043)
- Drop oxy-semantic's last three unreachable parser helpers (#3049)

### 📚 Documentation

- Update product-context.md from recent changelogs (#3041)
- Refresh ownership matrix + CODEOWNERS (weekly) (#3039)

### ⚙️ Miscellaneous Tasks

- Remove weekly health review workflow
- Add scripts/conn-leak, a read-only detector for Postgres pool exhaustion (#3060)
- Release 0.5.134 (#3045)

## [0.5.133] - 2026-08-31

### 🚀 Features

- Oxy worker drives runs (Phase 1) — additional driver, direct-drive untouched (#3014)
- The admin app view is a URL, and the preview stops eating the Back button (#3024)

### 🐛 Bug Fixes

- The collapse step matched a bot login GraphQL never returns (#3026)
- Deliver live events across pods, not just in-process (#2823)

### 🚜 Refactor

- Peel the eval crate off the old executor pipeline (retirement, Phase 2) (#3006)
- Relocate execute::types to oxy::exec_types (retirement, Phase 3) (#3025)

### 📚 Documentation

- Uploaded reports are the second non-re-extractable source (#3029)
- A durable bronze of files is not automatically replayable (#3040)

### ⚡ Performance

- Let bundles read pre-aggregations, and stop scanning system.columns on every query (#3013)

### ⚙️ Miscellaneous Tasks

- Collapse superseded Claude review comments (#3022)
- Scope the review bot to the PR diff and its side effects (#3027)
- *(airway)* Bump to 0.1.37 — filesystem OOM fix, postgres TLS, SP-API (#3030)
- Release 0.5.133 (#3023)

## [0.5.132] - 2026-08-28

### 🐛 Bug Fixes

- Ctx.tx and ctx.oltp errors said "db error" and nothing else (#3017)
- A custom app opens in its own tab, and can actually be driven there (#3020)

### 💼 Other

- *(deps)* Bump rui314/setup-mold from 9c9c13bf4c3f1adef0cc596abc155580bcb04444 to 7e4f20ad28a2e8ca6fd0892ccf72e2abb706b9c3 (#3009)
- *(deps)* Bump astral-sh/setup-uv from 9.0.0 to 10.0.1 (#3008)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 8 updates (#3011)
- *(deps)* Bump the prod-npm-minor-dependencies group across 2 directories with 11 updates (#3010)

### 🚜 Refactor

- Make the working copy a capability, and absence loud (#2822)

### ⚙️ Miscellaneous Tasks

- Release 0.5.132 (#3012)

## [0.5.131] - 2026-08-26

### 🚀 Features

- Derive an uploadable source's landing zone instead of declaring it (#2992)
- Generate HTTP route catalog from router source at build time (#2999)
- Let a schedule carry variables, and stop the form erasing them (#3005)
- Make pre-aggregation work across the fleet, and surface it in the factory (#2989)
- Per-org OLTP Postgres — provisioning, writer isolation, and the admin console (#2851)

### 🐛 Bug Fixes

- Stop a semantic query silently answering from the wrong warehouse (#2979)
- Render semantic pre-aggregations from the declaration, not the worker (#2988)
- Close the second review pass on the derived landing zone (#2998)

### 💼 Other

- Pin airway to the 0.1.34 tag (#2995)

### 🚜 Refactor

- Extract onboarding into a sibling crate + build the workspace seam (#2996)
- Begin retiring the old executor — plan + eval target on agentic (#3003)

### ⚙️ Miscellaneous Tasks

- *(airway)* Bump to 0.1.35 — keyless children and employee propagation (#3000)
- Release 0.5.131 (#2997)

## [0.5.130] - 2026-08-24

### 🚀 Features

- Google_sheets source for airway (#2945)
- External liveness endpoint for a published custom app (#2968)
- Custom-app user identity — ctx.user profile/role/teams, and role capture in usage tracking (#2966)
- An admin console for the Airhouse fleet (#2970)
- Show who is viewing the seeded starter app (#2975)
- Make the Airhouse console answer the question it was opened for (#2976)
- Custom apps that load instantly, and report how they're used (#2983)

### 🐛 Bug Fixes

- Compile with --no-default-features by ungating the custom-app function seam (#2960)
- Stop the analytics agent answering confidently from the wrong rows (#2969)
- Teach oxy's MeasureType the two aggregations airlayer already knows (#2987)
- Web-app chat panel state leaking across workspace switches (#2974)
- Agentic runner resolves snapshot refs via aria-ref locator (#2990)
- Thinking config still targets the Claude 4 API (#2986)

### 💼 Other

- Drop V8 under --no-default-features by flipping github's oxy-app dep (#2972)
- Bump pnpm dependencies to their latest major versions (#2965)
- Bump cargo dependencies to their latest major versions (#2977)

### 🚜 Refactor

- Remove redundant rustdoc and banner decoration lines (#2964)
- Streamline LLM configuration descriptions in JSON schemas
- Extract partner-console into a sibling crate (oxy-api-partner-console) (#2978)

### 📚 Documentation

- Refresh ownership matrix + CODEOWNERS (weekly) (#2993)
- Update product-context.md from recent changelogs (#2994)

### ⚡ Performance

- *(build)* Composition inversion + opt-in dynamic linking for a faster dev loop (#2961)

### ⚙️ Miscellaneous Tasks

- Run every test job on warp arm64, and keep agentic tests off the public mirror (#2967)
- Harden Playwright browser setup against apt mirror flakiness (#2971)
- Make dependency upgrades verifiable instead of hopeful (#2980)
- Release 0.5.130 (#2963)

## [0.5.129] - 2026-08-20

### 🐛 Bug Fixes

- Workspace metric-tree 500s on the serve fleet (#2946)

### 💼 Other

- Pin airway to the 0.1.33 tag (#2950)
- *(deps)* Bump Swatinem/rust-cache from e18b497796c12c097a38f9edb9d0641fb99eee32 to f0d9c3887740aee45f6153b24b3a6b815192ec16 (#2956)
- *(deps)* Bump github/codeql-action/upload-sarif from 4.37.0 to 4.37.7 (#2955)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 5 updates (#2958)

### 🚜 Refactor

- Remove restating comments across crates and web-app (follow-up to #2954) (#2959)

### ⚙️ Miscellaneous Tasks

- Remove redundant inline comments throughout codebase (#2954)
- Release 0.5.129 (#2951)

## [0.5.128] - 2026-08-19

### 🚀 Features

- *(ubereats)* Report upload UI, with the bucket kept server-side (#2942)

### ⚙️ Miscellaneous Tasks

- Release 0.5.128 (#2947)

## [0.5.127] - 2026-08-18

### 🚀 Features

- Make ClickHouse the sole observability backend (#2676)
- Anomaly inbox — action feedback, paging, batch triage (#2914)
- Dev-only sign-in bypass for browser automation (#2933)
- *(airway)* Bump to 0.1.30 and wire the ubereats source (#2937)
- *(airway)* Wire the NetSuite source (#2939)
- Ctx.tx() — atomic multi-statement writes for Oxy Functions (#2944)

### 🐛 Bug Fixes

- Raise the delegation suspend timeout to 4h, sized from measured runs (#2929)

### 💼 Other

- Bump airway 0.1.28 -> 0.1.29 for delete-by-parent on Merge children (#2932)

### 📚 Documentation

- Re-anchor two observability comments left pointing at removed backends (#2931)
- Update product-context.md from recent changelogs (#2941)
- Refresh ownership matrix + CODEOWNERS (weekly) (#2940)
- Aim the PR template's comprehension check at design decisions (#2936)
- *(healthcheck)* Stop recommending /health as a liveness probe (#2943)

### ⚡ Performance

- Seed a fresh checkout's target/ from a warm one (cold build 418s -> ~61s) (#2935)

### 🧪 Testing

- Name the lost claim, and write down why the queue tests are fragile (#2934)

### ⚙️ Miscellaneous Tasks

- Release 0.5.127 (#2930)

## [0.5.126] - 2026-08-13

### 🐛 Bug Fixes

- Keep a suspended task's heartbeat alive so the reaper can't re-run it (#2927)

### 💼 Other

- *(deps)* Bump the prod-npm-major-dependencies group across 2 directories with 1 update (#2925)
- Bump airway 0.1.27 -> 0.1.28 for journal_entries + the Replace-child fix (#2926)

### ⚙️ Miscellaneous Tasks

- Upgrade deps
- Release 0.5.126 (#2928)

## [0.5.125] - 2026-08-13

### 🐛 Bug Fixes

- Stop the hourly SIGSEGV crash loop in the workspace-health smoke pass (#2916)

### 💼 Other

- Bump airway 0.1.26 -> 0.1.27 for the merge dedup + time_entries cursor lag (#2920)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 1 update (#2924)
- *(deps)* Bump sigstore/cosign-installer from 3.9.1 to 4.1.2 (#2922)
- *(deps)* Bump slackapi/slack-github-action from 3.0.3 to 4.0.0 (#2921)

### ⚙️ Miscellaneous Tasks

- Release 0.5.125 (#2919)

## [0.5.124] - 2026-08-12

### 🚀 Features

- Adopt airway 0.1.24 (#2883)
- *(video-poc)* Dedicated audio role + audio-capture stall watchdog + verdict log (#2912)
- Read-only QuickBooks token custody for airway pipelines (#2913)
- Adopt Sea-ORM 2.0 typed COLUMN + dependency upgrades (#2911)

### 🐛 Bug Fixes

- Make an unresolvable airway pipeline_ref retryable, not caller error (#2906)
- Gate the Airway nav entry on the capability its endpoint uses (#2909)
- Stop Home auto-loading GitHub onboarding (and its IdeOnly probe) (#2908)

### 💼 Other

- Bump airway 0.1.25 -> 0.1.26 for the Toast orders restatement window (#2917)

### 🚜 Refactor

- Restructure PR template for clarity and reviewer guidance (#2918)

### ⚙️ Miscellaneous Tasks

- Upgrade sea-orm to 2.0 and sqlx to 0.9 (#2900)
- Migrate workflow telemetry action to cilium fork (#2910)
- Release 0.5.124 (#2907)

## [0.5.123] - 2026-08-11

### 🚀 Features

- Resolve airway admission from per-source-kind config (#2864)
- Add the /admin/airway admission policy surface (#2875)
- *(queue)* Delayed visibility + airway single-flight redesign — serialize instead of reject (#2902)
- Custom-app asset lifecycle — retention, quotas, metering, seed + admin Storage tab (#2870)
- Airway contract consumer + deployment tier (#2881)

### 🐛 Bug Fixes

- *(video-poc)* Plumb UPSELL_CAMERAS through the edge install + compose (#2903)
- Read airway pipeline YAML from the compile boundary (#2876)
- Apply airway admission to automation-dispatched runs (#2880)

### ⚙️ Miscellaneous Tasks

- Release 0.5.123 (#2901)

## [0.5.122] - 2026-08-10

### 🚀 Features

- *(airway)* Upgrade to 0.1.23 and enforce contract admission (stage 1) (#2853)

### 🐛 Bug Fixes

- Three single-flight lease gaps found on dev (#2887)

### ⚙️ Miscellaneous Tasks

- Upgrade deps
- Update dependencies (#2899)
- Release 0.5.122 (#2897)

## [0.5.121] - 2026-08-10

### 🚀 Features

- Platform standing becomes a capability grant, not a boolean (#2873)
- *(cameras)* Add nullable label column to oxy_cam_events (#2849)
- *(video-poc)* Upsell-detection feasibility POC + design spec (#2847)
- Airway single-flight lease — one run per pipeline per workspace (#2861)
- Global admins can delegate app operators, bounded by may_delegate (#2884)
- *(video-poc)* Wire upsell audio detection into the edge worker (Phase 2) (#2886)
- *(video-poc)* Report upsell AudioReader liveness via a health loop (#2889)

### 🐛 Bug Fixes

- *(edge)* Stop phantom annotation boxes — .predict() not .track() (#2860)
- Blank page on back-navigation from a custom app (#2863)
- Worker /metrics queue-depth query selected a nonexistent column (#2877)
- Don't serve Oxy Functions as assets; warn on untraceable app builds (#2879)
- *(video-poc)* Center-anchor zone counting + relax congestion capture (#2888)
- *(monitors)* Band gate survives contamination, severity ranks on headroom, cohorts group a chain-wide drop (#2859)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 10 updates (#2869)
- *(deps)* Bump taiki-e/install-action from 2.83.1 to 2.85.7 (#2867)
- *(deps)* Bump astral-sh/setup-uv from 7.6.0 to 9.0.0 (#2866)

### 📚 Documentation

- Record the ses:SendRawEmail deploy prerequisite (#2855)
- Update product-context.md from recent changelogs (#2896)
- Refresh ownership matrix + CODEOWNERS (weekly) (#2895)

### ⚡ Performance

- Publish-time brotli and zero per-asset DB queries on the customer-apps serve plane (#2878)
- Cut oxy-app test-suite cost — 31 test binaries to 5, per-run schema template (#2891)
- Group the remaining crates' integration tests into one binary each (#2893)

### ⚙️ Miscellaneous Tasks

- Remove the cargo-deny supply-chain job (#2874)
- Weekly health review — 2026-08-10 (#2892)
- Release 0.5.121 (#2856)

## [0.5.120] - 2026-08-04

### 🐛 Bug Fixes

- Settings sections a Member can't use no longer appear (#2848)
- Custom-app email attachments arrive corrupt because SES emitted 7bit (#2852)

### ⚙️ Miscellaneous Tasks

- Release 0.5.120 (#2850)

## [0.5.119] - 2026-08-03

### 🚀 Features

- *(cameras)* Expose clip-playback URL on the external API surface (#2830)
- *(edge)* Annotate congestion clips with zone, tracked boxes, live count (#2841)
- *(workspace-health)* Make health checks opt-in per workspace (#2838)
- *(custom-apps)* Team-based app visibility with org, admin and partner controls (#2836)

### 🐛 Bug Fixes

- *(custom-apps)* Make email attachments actually sendable from a function (#2842)
- *(reconcile)* Surface the compared window, document the sub-grain freshness cliff (#2821)
- *(anomalies)* Separate causal, offsetting, and mechanical drivers in explain (#2834)

### 📚 Documentation

- Biweekly internal-docs distillation (#2840)
- Add customer-apps publishing quickstart guide (#2843)
- Update product-context.md from recent changelogs (#2846)
- Refresh ownership matrix + CODEOWNERS (weekly) (#2845)

### ⚙️ Miscellaneous Tasks

- Weekly health review — 2026-08-03 (#2844)
- Release 0.5.119 (#2835)

## [0.5.118] - 2026-07-30

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 8 updates (#2827)
- *(deps)* Bump anthropics/claude-code-action from 1.0.174 to 1.0.183 (#2825)
- *(deps)* Bump actions/checkout from 7.0.0 to 7.0.1 (#2824)
- *(deps)* Bump prod npm minors + jsdom to latest (consolidates #2826, #2828) (#2831)

### 🚜 Refactor

- *(custom-apps)* Repoint authz imports at oxy-server-authz (6→4 seams) (#2804)
- *(custom-apps)* Invert the OxyProjectContext consumption behind a trait (#2803)

### 📚 Documentation

- Establish team ownership matrix and CODEOWNERS (#2833)

### ⚙️ Miscellaneous Tasks

- Release 0.5.118 (#2829)

## [0.5.117] - 2026-07-29

### 🚀 Features

- *(monitors,reconcile)* Timezone-aware windows + freshness watermark (#2811)

### 🐛 Bug Fixes

- *(webhooks)* Resolve the Toast integration from the compile boundary (#2816)
- *(git)* Never fall back to the host machine's git credentials (#2818)
- *(health)* Re-alert while a workspace stays unhealthy, link to its Health tab (#2812)
- *(workspaces)* Report an unmaterialised working copy as a transient 503 (#2814)

### ⚙️ Miscellaneous Tasks

- *(skills)* Reconcile knowledge cards with skills@0c51d931 (#2815)
- Release 0.5.117 (#2817)

## [0.5.116] - 2026-07-28

### 🚀 Features

- *(world-model)* Opportunity sizing — sized segments, recursive drill & drivers on measures (+ airlayer rate-based additive sizing #78) (#2715)

### 🐛 Bug Fixes

- *(edge)* Reach MediaMTX playback by service name, not 127.0.0.1 (#2810)
- *(fleet)* Carry public host across the ide proxy hop (#2813)

### 🚜 Refactor

- *(cameras)* Move clip S3 env to OXY_CAMERAS_CLIPS_S3_* namespace (#2801)

### ⚙️ Miscellaneous Tasks

- Release 0.5.116 (#2809)

## [0.5.115] - 2026-07-27

### 🚀 Features

- *(cameras)* Evidence clips — edge congestion detector + server persistence (+ ByteTrack) (#2796)
- *(api)* Expose anomaly scan + explain on the external API surface (#2798)

### 🐛 Bug Fixes

- *(fleet)* Pin /semantic/world-model routes IdeOnly (#2806)

### 📚 Documentation

- Warehouse decision record + internal-docs distillation & biweekly upkeep workflow (#2793)
- Medallion architecture + video-analytics data schema designs (#2752)
- Update product-context.md from recent changelogs (#2802)

### ⚙️ Miscellaneous Tasks

- Weekly health review — 2026-07-27 (#2800)
- Release 0.5.115 (#2799)

## [0.5.114] - 2026-07-27

### 🚀 Features

- Workspace smoke test as a health-monitoring dimension (#2704)

### 🐛 Bug Fixes

- Make anomaly explain seasonality- and segment-aware (#2770)
- Compile semantic queries from the boundary, not the working copy (#2795)

### 🚜 Refactor

- *(custom-apps)* Invert the projects::query seam behind a trait (#2789)

### 🧪 Testing

- *(custom-apps)* Pin the surface's module boundary with a source-scan test (#2784)

### ⚙️ Miscellaneous Tasks

- Release 0.5.114 (#2794)

## [0.5.113] - 2026-07-26

### 🚀 Features

- *(admin)* Quick-create org + onboard owner; onboarding org switcher (never blocked) (#2792)
- Assume an org's role in one click from the admin app page (#2782)
- Make admin app-detail dossier dockable and denser (#2783)
- *(cli)* Oxy assume (assume-role) + --env accepts a pasted URL (#2791)

### 🐛 Bug Fixes

- *(create-oxy-app)* Scaffold a custom app, not a backend project (#2769)
- *(custom-apps)* Close two publish P0s + three silent-publish P1s (#2790)
- *(invitations)* A lapsed invite permanently blocks re-inviting that address (#2788)

### 🚜 Refactor

- *(project.yml)* Update project configuration for serena

### 📚 Documentation

- Restructure IA around World Model, Agentic Intelligence, and the SDK (#2736)
- *(custom-apps)* Platform gap analysis + safe diagnosability/cache fixes (#2787)

### ⚙️ Miscellaneous Tasks

- *(create-oxy-app)* Release 0.4.0 (#2781)
- Cargo cliipy and fmt
- Release 0.5.113 (#2780)

## [0.5.112] - 2026-07-24

### 🚀 Features

- *(authz)* Explain the staff assume-role 403 instead of denying opaquely (#2777)

### 🐛 Bug Fixes

- *(shell)* Drop the workspace-tile fill behind a real logo (#2771)
- *(workspace-health)* Stop the health check counting its own eval runs (#2776)

### 🚜 Refactor

- *(authz)* Extract oxy-server-authz crate (+ widen the security-boundary test) (#2772)

### ⚙️ Miscellaneous Tasks

- *(claude)* Allow kubectl without a per-call prompt (#2778)
- Release 0.5.112 (#2775)

## [0.5.111] - 2026-07-24

### 🚀 Features

- *(customer-apps)* Asset store (ctx.storage), email attachments, and per-app roles (#2768)

### 🚜 Refactor

- Rename "customer apps" → "custom apps" (unify the two names) (#2774)

### ⚙️ Miscellaneous Tasks

- Release 0.5.111 (#2773)

## [0.5.110] - 2026-07-23

### 🚜 Refactor

- *(app)* Rung-1 module splits of oxy-app god files (batch 2) (#2761)

### ⚙️ Miscellaneous Tasks

- Upgrade dependencies (apache-arrow, elkjs, testing-library) (#2766)
- Release 0.5.110 (#2767)

## [0.5.109] - 2026-07-23

### 🚀 Features

- Universalize the customer-app shell (sdk 2.5.0) (#2687)

### 🐛 Bug Fixes

- Surface anomaly segments and scan failures in the Insights Inbox (#2758)

### 💼 Other

- *(deps)* Bump softprops/action-gh-release from 3.0.1 to 3.0.2 (#2763)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 10 updates (#2765)
- *(deps)* Bump pnpm/action-setup from 5.0.0 to 6.0.9 (#2762)

### 🚜 Refactor

- *(app)* Rung-1 module splits of oxy-app god files (batch 1) (#2753)

### ⚙️ Miscellaneous Tasks

- Update dependencies in TypeScript SDK and web app
- Release 0.5.109 (#2759)

## [0.5.108] - 2026-07-22

### 🚀 Features

- *(agentic)* Check_data_freshness tool + preserve view meta through the shim (#2717)
- *(customer-apps)* Send email from Oxy Functions via ctx.email.send (#2740)
- *(runtime)* Worker eviction safety for the durable task queue (#2743)
- *(cli)* Restore oxy test for .test.yml files (#2749)
- Airway automation task type + trusted-load-completion consumption (#2747)
- Sdk shell universalization — @oxy-hq/sdk/shell + backend (#2745)
- *(models)* Add openai_compat vendor and honor explicit llm.vendor (#2751)

### 🐛 Bug Fixes

- Make world model, semantic query, and metric tree IDE surfaces branch-aware (#2750)
- *(ide)* Stop reporting false workspace sync/compile status (#2754)
- Handle non-release tags in version bump script (#2756)

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group across 2 directories with 2 updates (#2728)

### 🚜 Refactor

- *(app)* Decompose oxy-app by forcing function (supersedes #2739) (#2748)

### ⚙️ Miscellaneous Tasks

- *(sdk)* Release @oxy-hq/sdk 2.3.0 (/email subpath) (#2742)
- Expand agentic autofix scope: P0/P1 fixes + P3 refactors, add test coverage (#2755)
- Release 0.5.108 (#2737)

## [0.5.107] - 2026-07-18

### 🚀 Features

- *(partner)* Partner distribution platform + oxy proxy local-dev (#2710)
- *(cameras)* Route dashboard reads to the analytics DP pool (#2733)
- *(authz)* [**breaking**] Unify authorization behind one model (enforcing) (#2730)
- *(seed)* Deploy an example customer app and test the seed against a real DB (#2734)

### 🐛 Bug Fixes

- *(cameras)* Rebuild the tenant airhouse client when its driver dies (#2729)
- *(cameras)* Stop retrying a rejected ingest batch forever (#2732)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 2 updates (#2727)

### ⚙️ Miscellaneous Tasks

- Update dependencies across multiple packages
- Release 0.5.107 (#2726)

## [0.5.106] - 2026-07-16

### 💼 Other

- *(deps)* Bump actions/setup-node from 6.4.0 to 7.0.0 (#2720)
- *(deps)* Bump anthropics/claude-code-action from 1.0.171 to 1.0.174 (#2721)

### ⚙️ Miscellaneous Tasks

- Release 0.5.106 (#2724)

## [0.5.105] - 2026-07-15

### 🐛 Bug Fixes

- *(observability)* Render ClickHouse trace timestamps with %i, not %M (#2718)

### ⚙️ Miscellaneous Tasks

- Release 0.5.105 (#2719)

## [0.5.104] - 2026-07-15

### 🚀 Features

- Detect new deploys and recover stale-chunk route errors (#2716)

### 🐛 Bug Fixes

- Deleted-workspace health-check DLQ pile-up + default cadence to 1h (#2714)

### 📚 Documentation

- Update product-context.md from recent changelogs (#2712)

### ⚙️ Miscellaneous Tasks

- Weekly health review — 2026-07-13 (#2711)
- Release 0.5.104 (#2713)

## [0.5.103] - 2026-07-12

### 🚀 Features

- Admin apps cockpit + Access-tab revamp (#2700)
- Oxy Function Jobs — a Kestra subset on the orchestrator (extends #2685) (#2702)
- *(observability)* ClickHouse execution rollup + technical UI + local stack (#2698)
- *(observability)* Theme 3 traces console — search, ranges, live-tail, table, compare (#2706)

### 🐛 Bug Fixes

- *(db)* Drop oxydb pool min_connections 20→2, make bounds env-configurable (#2703)
- *(onboarding)* Resolve readiness LLM-key presence from the secret store (#2705)
- *(observability)* Harden trace serving + guard invalid-date crash (#2709)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-major-dependencies group across 2 directories with 1 update (#2696)

### ⚙️ Miscellaneous Tasks

- Implement weekly architecture & health review fixes (P0–P4) (#2707)
- Unified Weekly Health Review workflow (+ restore dev .env.airhouse/.clickhouse) (#2708)
- Release 0.5.103 (#2701)

## [0.5.102] - 2026-07-09

### 🚀 Features

- *(airhouse)* Per-workspace catalog-index toggle + honor untyped on execute_sql_query (#2688)
- Scheduled Oxy Functions (cron-triggered background functions + ctx.secrets.set) (#2685)

### 🐛 Bug Fixes

- Reconcile raw-sql checks resolve non-airhouse warehouses (#2690)

### 💼 Other

- *(deps)* Bump lewagon/wait-on-check-action from 1.8.0 to 1.8.1 (#2693)
- *(deps)* Bump the prod-npm-minor-dependencies group across 2 directories with 21 updates (#2694)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 10 updates (#2695)

### ⚙️ Miscellaneous Tasks

- *(sdk)* Bump @oxy-hq/sdk to 2.2.0 (#2692)
- Release 0.5.102 (#2691)

## [0.5.101] - 2026-07-08

### 🚀 Features

- *(airway)* Reset-schema (drop tables + clear stored schema) (#2666)
- *(ide-db)* Group Database sidebar by schema + drop schema/table (#2660)
- Admin custom-apps management revamp + unified app icon/art (#2689)
- World-model graph drill-down, breakdowns, and filter counts (#2684)

### ⚙️ Miscellaneous Tasks

- Release 0.5.101 (#2686)

## [0.5.100] - 2026-07-07

### 🚀 Features

- Revamp admin custom-apps into a manageable deployment-platform UI (#2682)

### ⚙️ Miscellaneous Tasks

- Release 0.5.100 (#2683)

## [0.5.99] - 2026-07-06

### 🚀 Features

- Harden airhouse observability — day partitioning + snapshot expiry (#2679)

### 🐛 Bug Fixes

- Observability airhouse poisoned-session recovery + bounded traces query (#2674)
- Flush observability spans every 30s instead of every second (#2677)

### 📚 Documentation

- Update product-context.md from recent changelogs (#2678)

### ⚙️ Miscellaneous Tasks

- Release 0.5.99 (#2675)

## [0.5.98] - 2026-07-06

### 🚀 Features

- Reconcile generalize to actual/expected operands + oxy/external split (#2661)

### 📚 Documentation

- Add CLI installation guide (#2673)

### ⚙️ Miscellaneous Tasks

- Update docs domain
- Cargo fmt and build
- Release 0.5.98 (#2670)

## [0.5.97] - 2026-07-05

### 🚀 Features

- *(airway)* Reset-in-place retry + run-scoped resume state (P1-P2c) (#2662)

### 📚 Documentation

- Optimize CLAUDE.md setup for agents (#2667)

### ⚡ Performance

- Customer-apps serve + data-plane wins (lean, stacked on Functions) (#2634)

### ⚙️ Miscellaneous Tasks

- Weekly auto-fix 2026-07-05 (#2669)
- Release 0.5.97 (#2665)

## [0.5.96] - 2026-07-04

### 🚀 Features

- Add Oxy Functions — edge-style TypeScript handlers in customer apps (#2521)

### ⚙️ Miscellaneous Tasks

- Release 0.5.96 (#2663)

## [0.5.95] - 2026-07-03

### 🚀 Features

- *(airway)* Generic resumable chunked backfill + coverage (#2644)

### 🐛 Bug Fixes

- *(customer-apps)* Wire project-scoped secrets into build_project_context (#2659)

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group across 2 directories with 28 updates (#2650)
- *(deps)* Bump crate-ci/typos from 1.47.2 to 1.48.0 (#2649)

### ⚙️ Miscellaneous Tasks

- Release 0.5.95 (#2658)

## [0.5.94] - 2026-07-02

### 🚀 Features

- App publish tokens for machine auth (publish + read scoped) (#2655)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 10 updates (#2656)
- *(deps-dev)* Bump the dev-npm-major-dependencies group across 2 directories with 1 update (#2652)

### ⚙️ Miscellaneous Tasks

- Release 0.5.94 (#2657)

## [0.5.93] - 2026-07-02

### 🚀 Features

- *(automation)* Add `http_request` workflow task type (#2643)
- Sdk streamline customer-app launcher-card screenshots (#2654)

### ⚙️ Miscellaneous Tasks

- Add scripts + docs to connect to cloud airhouse (#2632)
- Exclude scripts/airhouse from push
- Release 0.5.93 (#2653)

## [0.5.92] - 2026-07-01

### 🚀 Features

- Workspace health monitoring (per-workspace checks + reconciliation) (#2637)

### 🐛 Bug Fixes

- Generalize large-query result guards beyond ClickHouse (#2641)

### 📚 Documentation

- Update installation URLs to use new domain for edge and stable releases

### ⚙️ Miscellaneous Tasks

- Release 0.5.92 (#2642)

## [0.5.91] - 2026-06-29

### 🐛 Bug Fixes

- Scope serve port env vars (OXY_HTTP_PORT) to dodge k8s service-link collision (#2639)

### 📚 Documentation

- Update product-context.md from recent changelogs (#2638)

### ⚙️ Miscellaneous Tasks

- Release 0.5.91 (#2640)

## [0.5.90] - 2026-06-29

### 🚀 Features

- Configurable dev ports + oauth bounce proxy for multi-instance local dev (#2630)
- Admin Tenant 360 investigation cockpit (#2627)

### 🐛 Bug Fixes

- Redirect unknown org-subdomain hosts to the app host (#2628)
- Stop unbounded ClickHouse IDE queries from OOM-killing the pod (#2636)
- Web-app world model loading state (#2633)

### ⚙️ Miscellaneous Tasks

- Release 0.5.90 (#2635)

## [0.5.89] - 2026-06-28

### 🐛 Bug Fixes

- Agentic disable analytics-to-builder auto-delegation (#2625)

### ⚙️ Miscellaneous Tasks

- Release 0.5.89 (#2626)

## [0.5.88] - 2026-06-27

### 🐛 Bug Fixes

- Web-app reset semantic Explorer state when switching view/topic (#2620)
- Let operators view any tenant's thread from admin explorer (#2622)
- Hydrate SPA auth from session cookie on org subdomains (#2624)

### ⚙️ Miscellaneous Tasks

- Release 0.5.88 (#2623)

## [0.5.87] - 2026-06-27

### ⚙️ Miscellaneous Tasks

- Bump airlayer to 0.3.4 (boolean filter typed literals) (#2618)
- Release 0.5.87 (#2621)

## [0.5.86] - 2026-06-27

### ⚙️ Miscellaneous Tasks

- Bump airlayer to 0.3.3 (default query row limit) (#2616)
- Release 0.5.86 (#2617)

## [0.5.85] - 2026-06-26

### 🚀 Features

- Demo world model + fix dark-mode zoom controls (#2615)

### 🐛 Bug Fixes

- *(compile-boundary)* Resolve semantic views/topics by file_path, not name (#2613)

### ⚙️ Miscellaneous Tasks

- Release 0.5.85 (#2614)

## [0.5.84] - 2026-06-25

### 🚀 Features

- Add org subdomain route for read-only access
- Web-app universal top bar, docked Ask panel, chat landing (#2597)

### 💼 Other

- *(deps)* Bump actions/checkout from 6 to 7 (#2610)
- *(deps)* Bump actions/cache from 5 to 6 (#2609)
- *(deps)* Bump the prod-npm-minor-dependencies group across 1 directory with 10 updates (#2611)

### ⚙️ Miscellaneous Tasks

- Cargo clippy and format codes
- Update deps
- Release 0.5.84 (#2608)

## [0.5.83] - 2026-06-24

### 🚀 Features

- *(world-model)* World Model graph — entity/measure visualization & instance drill-down (#2582)
- Org subdomain routing for select customer orgs (#2587)

### 🐛 Bug Fixes

- *(cameras)* Coalesce edge ingest writes to airhouse (one commit/window, not per-POST) (#2604)

### 🚜 Refactor

- Rename workflow/procedures to automation (#2593)

### 🧪 Testing

- Verify per-target init lock prevents duplicate pool slots (#2606)

### ⚙️ Miscellaneous Tasks

- Bump airway to 0.1.15 (#2607)
- Release 0.5.83 (#2605)

## [0.5.82] - 2026-06-24

### 🐛 Bug Fixes

- Ignore .worktrees/ locally so worktree creation never dirties main (#2598)

### ⚙️ Miscellaneous Tasks

- Bump airway to 0.1.14 (#2603)
- Release 0.5.82 (#2602)

## [0.5.81] - 2026-06-23

### 🐛 Bug Fixes

- Airhouse connection management — analytics connector pool + cameras reconnect flap-bound (#2596)

### ⚙️ Miscellaneous Tasks

- Release 0.5.81 (#2599)

## [0.5.80] - 2026-06-23

### 🐛 Bug Fixes

- Don't write oxy.log in cloud; bound it on local (#2591)
- Reuse persistent per-tenant Airhouse connection for camera ingest (#2592)

### 📚 Documentation

- Update product-context.md from recent changelogs (#2588)

### ⚙️ Miscellaneous Tasks

- Release 0.5.80 (#2595)

## [0.5.79] - 2026-06-22

### 🚀 Features

- *(airway)* Manual date-window backfill endpoint (Toast/QuickBooks) + bump airway 0.1.10 (#2576)

### 🐛 Bug Fixes

- Web-app load semantic view/topic preview from active IDE branch (#2585)

### ⚙️ Miscellaneous Tasks

- Bump airway to 0.1.12 (#2579)
- Add timeout-minutes guardrails to all workflow jobs (#2586)
- *(airway)* Toast orders → replacing + end-of-load compaction (#2584)
- Release 0.5.79 (#2577)

## [0.5.78] - 2026-06-19

### 🚀 Features

- *(app)* Add world-model LLM proxy endpoint for the voice assistant (#2553)
- *(serve)* Compile-complete stateless serving + ephemeral-env foundations (plan-2) (#2557)
- Show Airhouse server version in settings (#2574)

### 🐛 Bug Fixes

- Unified secrets table (#2556)

### 📚 Documentation

- Fix broken internal links (404s) in /guide pages (#2568)

### ⚙️ Miscellaneous Tasks

- Remove emojis from edge UI copy (#2561)
- Release 0.5.78 (#2571)

## [0.5.77] - 2026-06-18

### 🚀 Features

- Apps-first shell — HQ home, icon rail, global Ask drawer (#2497)

### 💼 Other

- *(deps)* Bump lewagon/wait-on-check-action from 1.7.0 to 1.8.0 (#2558)
- *(deps)* Bump the prod-npm-minor-dependencies group across 2 directories with 27 updates (#2559)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 9 updates (#2560)

### 📚 Documentation

- Remove SQLite to PostgreSQL migration guide (#2563)
- Update Oxygen product description to OS positioning (#2565)
- Finish OS positioning cleanup (changelog + agent context) (#2566)
- Correct inaccurate API/CLI claims and add positioning guardrails (#2567)

### ⚙️ Miscellaneous Tasks

- Release 0.5.77 (#2562)

## [0.5.76] - 2026-06-17

### 🐛 Bug Fixes

- *(airway)* Align overpass test to airway's {bbox} placeholder (#2552)
- Custom-app auth — clear cookie on logout + honor return_to after OAuth (#2555)

### ⚙️ Miscellaneous Tasks

- Release 0.5.76 (#2554)

## [0.5.75] - 2026-06-16

### 🚀 Features

- *(compile)* Compile boundary end-to-end + routing guardrail (re-open of #2494 after revert) (#2505)
- Publish all app definitions (#2510)
- Add pagination and filtering to admin explorer (#2511)
- *(compile)* Compile-boundary production-safety harness (#2524)
- *(admin)* Compile console — aggregate by workspace, full IDs, batch ops + oxy api typed fields (#2525)
- *(serve)* Self-route IdeOnly requests to the ide pod (kill the external route table) (#2526)
- *(ci)* Create composite action for building oxy CLI with multi-target support

### 🐛 Bug Fixes

- *(compile)* Wire db into the global-driver project context (#2514)
- *(admin)* Drop trailing slash on admin/compiles list URL (#2516)
- *(compile)* Make the stateless serve fleet safe (no FS fallthrough) (#2520)
- *(vscode)* Change default formatter for JSONC files to Prettier
- Resolve airway rest_api auth secrets from the secret manager (#2527)
- *(serve)* Classify node-local-state routes IdeOnly for self-routing (#2528)
- /apps/source 404 on serve fleet + onboarding redirect on 'Start asking questions' (#2531)
- *(serve)* S3-mirrored DuckDB fails on the fleet (agent runs: 'no databases configured') (#2533)
- *(compile)* Key idempotent revision reuse on compiler + schema version (#2534)
- *(serve)* Classify all /modeling routes IdeOnly (GET + bare root) (#2536)
- *(agentic)* Resolve relative dates in a configured timezone, not UTC (#2539)
- *(serve)* Make agentic analytics runs work on the stateless serve fleet (#2543)
- *(serve)* List procedures from the compile boundary on the stateless fleet (#2544)
- *(airway)* Use the ***bbox*** placeholder in the overpass source test (#2545)
- *(serve)* Pin the agentic run/exec surface to the ide (tier 1) (#2547)
- *(serve)* Use the local DuckDB connector on FS-backed nodes (ide/all) (#2548)
- *(serve)* Route data-app run to the ide instance (#2549)
- *(serve)* Pin data-app load + result execution to the ide (#2550)

### 📚 Documentation

- Refocus product-context.md as a lean clarifier, not a catalog (#2529)
- Update product-context.md from recent changelogs (#2530)
- Restructure documentation for Oxygen Cloud platform (#2535)
- Use oxygen-hq.com domain and /guide deploy path (#2540)

### 🧪 Testing

- *(serve)* Router-derived completeness gate for workspace route classification (#2537)

### ⚙️ Miscellaneous Tasks

- Unify nextest profile and disable incremental compilation (#2515)
- Bump airway to 0.1.7 (#2517)
- Release 0.5.75 (#2509)

## [0.5.74] - 2026-06-12

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group across 2 directories with 35 updates (#2501)

### ⚙️ Miscellaneous Tasks

- Release 0.5.74 (#2508)

## [0.5.73] - 2026-06-11

### 🚀 Features

- *(external-api)* Expose anomaly endpoints on external API surface (#2489)
- *(airhouse)* Re-mint pipeline credentials on reconnect via CredentialProvider (#2496)
- *(site-scout)* Procedure-run wiring + airway pin chain + env vars in airway YAMLs (#2433)
- *(compile)* Compile boundary end-to-end — multi-instance ready (#2494)
- *(admin)* Cockpit revamp — internal jobs, custom apps, LLM cost dashboard, cross-tenant explorer (#2503)

### 🐛 Bug Fixes

- *(observability)* Add tcp keepalive to airhouse pgwire connection (#2490)
- *(edge)* Make MediaMTX camera publisher self-heal on stalled feeds (#2493)
- *(edge)* Use RTSP -timeout instead of -rw_timeout for publisher ffmpeg (#2495)

### 💼 Other

- *(deps)* Bump actions/checkout from 4 to 6 (#2500)
- *(deps)* Bump docker/metadata-action from 5 to 6 (#2499)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 7 updates (#2502)

### ⚙️ Miscellaneous Tasks

- Release 0.5.73 (#2492)

## [0.5.72] - 2026-06-10

### ⚙️ Miscellaneous Tasks

- Release 0.5.72 (#2488)

## [0.5.71] - 2026-06-09

### 🐛 Bug Fixes

- *(preagg)* Restore fallback config in serve workspace builder (#2407 regression, fixes 0.5.70 crash-loop) (#2484)

### ⚙️ Miscellaneous Tasks

- Release 0.5.71 (#2485)

## [0.5.70] - 2026-06-09

### 🚀 Features

- Metric tree integration — semantic layer tab, anomaly monitoring, and metric-tree agent tools (#2407)

### 🐛 Bug Fixes

- Customer-apps subdomain login redirect loops to 414 in incognito (#2482)

### ⚙️ Miscellaneous Tasks

- Release 0.5.70 (#2483)

## [0.5.69] - 2026-06-09

### 🚀 Features

- *(edge)* Tailscale funnel + JWT-only auth + UX polish (#2469)
- *(airway)* ClickHouse timeout, settings + batch_size config (airway 0.1.3) (#2479)

### 🐛 Bug Fixes

- *(admin)* Surface subdomain URL for S3 apps too, not just v0 (#2476)
- *(router)* Honor OXY_DATABASE_SSL_MODE in the LISTEN connector (fix chronic router.reconnect TLS handshake flap) (#2480)

### 🚜 Refactor

- Extract credential forms into separate components (#2473)

### ⚙️ Miscellaneous Tasks

- Bump airway to 0.1.3 (#2481)
- Release 0.5.69 (#2477)

## [0.5.68] - 2026-06-08

### 🐛 Bug Fixes

- Enforce active status and expiration checks on API key validation (#2471)
- *(logging)* Suppress raw SQL and TLS chatter under OXY_DEBUG (#2475)

### 📚 Documentation

- Update product-context.md from recent changelogs (#2474)

### ⚙️ Miscellaneous Tasks

- Release 0.5.68 (#2472)

## [0.5.67] - 2026-06-08

### 🚀 Features

- *(custom-apps)* Usage tracking — view events, custom events, Activity tab (#2465)

### 🐛 Bug Fixes

- Host dispatch double-prefixes S3-source app asset URLs → blank page (#2466)
- *(sdk)* Export useTrackEvent + bump @oxy-hq/sdk to 2.1.0 (#2468)

### ⚙️ Miscellaneous Tasks

- Run cargo fmt
- Release 0.5.67 (#2467)

## [0.5.66] - 2026-06-08

### 🚀 Features

- Add workspace path resolution for procedure SQL files (#2456)
- *(admin)* Custom-apps URL chips, /admin landing, version badge, rename (#2463)

### ⚙️ Miscellaneous Tasks

- Release 0.5.66 (#2458)

## [0.5.65] - 2026-06-05

### 🚀 Features

- Add AWS RDS IAM authentication support to worker (#2455)

### ⚙️ Miscellaneous Tasks

- Release 0.5.65 (#2457)

## [0.5.64] - 2026-06-05

### 🚀 Features

- Live operator console for internal jobs admin (#2450)
- *(customer-apps)* Publish org-id, multi-env login, delete cleans S3, new-vs-version (#2452)
- *(edge)* UniFi scan + install/auth diagnostics (#2448)

### 📚 Documentation

- Enhance API documentation with authentication and CLI guides (#2425)

### ⚙️ Miscellaneous Tasks

- Bump airway to 0.1.1 (#2453)
- Restrict edge image build to main repository (#2454)
- Release 0.5.64 (#2451)

## [0.5.63] - 2026-06-04

### 🚀 Features

- Clickhouse source for airway ELT pipeline (#2422)
- QuickBooks Online ELT source + OAuth Connect (#2432)
- Subdomain dispatch (zero-config) + dual-url admin UI for v0 customer apps (#2442)
- Standalone worker fleet + internal jobs admin (#2409)

### 🐛 Bug Fixes

- Silence spurious manifest error for v0 customer apps (#2441)
- Surface real connector error on customer-app query 500s (#2447)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.46.3 to 1.47.2 (#2444)
- *(deps)* Bump docker/setup-qemu-action from 3 to 4 (#2443)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 10 updates (#2445)
- *(deps-dev)* Bump the dev-npm-major-dependencies group across 2 directories with 4 updates (#2446)

### ⚙️ Miscellaneous Tasks

- Release 0.5.63 (#2440)

## [0.5.62] - 2026-06-03

### 🐛 Bug Fixes

- Skip worktree copies when discovering semantic files (#2437)

### ⚙️ Miscellaneous Tasks

- Release 0.5.62 (#2438)

## [0.5.61] - 2026-06-03

### 🐛 Bug Fixes

- Load config from worktree subdirectory on branch switch (#2434)
- Get competitor (#2436)

### ⚙️ Miscellaneous Tasks

- Release 0.5.61 (#2435)

## [0.5.60] - 2026-06-03

### 🚀 Features

- Customer-apps platform v2 + v0-flow alignment + cross-subdomain SSO (#2312)
- *(api)* Auto-allow self-origin in the customer-apps gate
- Weather data-enrichment POC (airway weather source + semantic layer) (#2406)
- *(customer-apps)* Reverse-proxy backend for remote-hosted bundles (#2428)
- World model (#2387)

### 🐛 Bug Fixes

- *(api)* Admin app URL is relative; drop OXY_CUSTOMER_APPS_BASE_URL
- *(web)* ResolveBundleUrl explicitly clears a stray port
- *(web)* Default channel to draft for unpublished customer apps

### 🚜 Refactor

- Simplify conditionals and remove unused imports across multiple files
- *(api)* Streamline CORS handling and improve origin checks

### 📚 Documentation

- Update product-context.md from recent changelogs (#2424)

### ⚡ Performance

- Optimize missingSlots computation with caching and bucketing (#2421)
- Batch event queries in run tree endpoint (#2420)

### ⚙️ Miscellaneous Tasks

- Add authentication for private cargo dependencies in weekly autofix workflow
- Monthly dead code cleanup 2026-06-01 (#2418)
- Publish @oxy-hq SDK packages to GitHub Packages (#2427)
- *(sdk)* Publish @oxy-hq SDK to public npm + fix package exports (#2429)
- Update domain from oxy.tech to oxygen-hq.com (#2405)
- *(sdk)* Make SDK publish workflow publish-only (PR-protected main) (#2430)
- Release 0.5.60 (#2419)

## [0.5.59] - 2026-05-29

### 🐛 Bug Fixes

- Stabilize agentic browser test suite on main (#2379)
- Add module type to DuckDB worker initialization (#2404)

### ⚙️ Miscellaneous Tasks

- Simplify git-cliff release notes generation (#2401)
- Release 0.5.59 (#2402)

## [0.5.58] - 2026-05-28

### 🚀 Features

- Cron scheduler — Phase 1 consumer + Phase 2 (backend + UI) (#2350)
- Airlayer build & pull integration for pre-aggregation support (#2334)
- Coordinator dashboard redesign + workspace-scoped metrics (#2380)
- Support git repositories with workspaces in subdirectories (#2400)

### 🐛 Bug Fixes

- Add missing example view (#2373)
- Pass workspace_id to insert_run in preagg worker (#2375)
- Handle chrono::Duration overflow in preagg Every interval (#2395)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 15 updates (#2392)
- *(deps)* Bump crate-ci/typos from 1.46.2 to 1.46.3 (#2390)
- *(deps)* Bump the prod-npm-minor-dependencies group across 2 directories with 15 updates (#2393)

### 🚜 Refactor

- Remove agentic workflows routing agents (#2346)
- Extract error handling logic in airwayReducer (#2398)
- Consolidate tool event emission and file mutation handling (#2399)
- Replace expect_sql() with safer sql() method in analytics (#2396)

### 📚 Documentation

- Revise welcome documentation for Oxygen OS
- Update product-context.md from recent changelogs (#2386)

### ⚙️ Miscellaneous Tasks

- *(skills)* Reconcile knowledge cards with skills@445c5459 (#2374)
- *(skills)* Reconcile knowledge cards with skills@6aa77a42 (#2376)
- *(skills)* Reconcile knowledge cards with skills@95c64756 (#2385)
- *(skills-reconcile)* Allow the oxyhelper bot to trigger reconcile (#2384)
- Restrict agentic browser tests to internal repository (#2394)
- Release 0.5.58 (#2371)

## [0.5.57] - 2026-05-21

### 🐛 Bug Fixes

- Observability drop CREATE INDEX DDL from airhouse backend (#2368)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.46.1 to 1.46.2 (#2362)

### ⚙️ Miscellaneous Tasks

- Release 0.5.57 (#2370)

## [0.5.56] - 2026-05-20

### 🚀 Features

- Platform cleanup - refactor workflow run to use orchestrator  runner (#2300)
- Airway ELT integration + streaming pipeline (#2337)
- Add Airhouse observability backend support (#2340)
- Agentic browser test suite + cloud-mode CI matrix job (#2275)
- Add Airhouse observability backend (#2356)
- Agentic browser test skill (migrated from oxy-hq/skills#11) (#2358)
- Data app publish paradigm (#2313)
- Web-app guard third-party renderers with ErrorBoundary (#2332)

### 🐛 Bug Fixes

- Optimize character search for redaction in redact_secrets function
- Downgrade windows-sys dependency versions for compatibility
- Update CI workflow to conditionally use ARM64 runner and disable certain jobs
- Update CI workflow to use consistent runner for ARM64 jobs
- Add periodic WAL checkpoints to bound DuckDB file growth (#2331)
- Improve semantic layer & events handling for builder  (#2343)
- Add logging for workflow file listing errors in OxyProcedureRunner (#2352)
- Scope onboarding wizard state per deployment in local mode (#2302)
- Ide git actions and monaco theme (#2330)
- Web app hidden bugs (#2333)
- Revert observability to env-var credentials (PR #2356 regression) (#2360)

### 📚 Documentation

- Update product-context.md from recent changelogs (#2344)

### ⚡ Performance

- Avoid cloning task_ids in coordinator hot path (#2353)
- Batch query run events to eliminate N+1 queries (#2354)

### ⚙️ Miscellaneous Tasks

- Remove dead code and unused dependencies in web-app (#2324)
- Remove unused Rust dependencies across 10 crates (#2326)
- Conditionally run disk space cleanup on GitHub-hosted runners
- *(skills-reconcile)* Trigger on repository_dispatch from skills (#2328)
- Weekly auto-fix 2026-05-17 (#2339)
- *(skills)* Reconcile knowledge cards with skills@f9ebd8af (#2345)
- Add GitHub App token auth for private cargo dependencies (#2351)
- Remove base parameter from paths-filter
- Update BUILD_DOCKER condition in public-release.yaml
- Use short SHA instead of date for edge image tags (#2357)
- Release 0.5.56 (#2325)

## [0.5.55] - 2026-05-14

### 🚀 Features

- Add airhouse local dev compose stack (#2301)
- Update dependencies and improve component configurations
- Add additional commands to permissions in settings.json

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.45.2 to 1.46.1 (#2315)
- *(deps-dev)* Bump the dev-npm-major-dependencies group across 1 directory with 2 updates (#2320)
- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 5 updates (#2322)

### ⚙️ Miscellaneous Tasks

- Update dependencies in sdk/typescript and web-app
- Simplify public release workflow and speed up builds (#2294)
- Release 0.5.55 (#2314)

## [0.5.54] - 2026-05-12

### 🚀 Features

- Web-app mobile usability and fix bugs (#2299)

### 🐛 Bug Fixes

- Web-app billing settings loading state (#2296)
- Surface billing feature flag in admin UI (#2295)

### 📚 Documentation

- Update product-context.md from recent changelogs (#2307)
- Update DEVELOPMENT.md to clarify HTTPS setup for local development and remove outdated sections
- Update DEVELOPMENT.md

### ⚙️ Miscellaneous Tasks

- Clean up formatting and comments in Claude Code Review workflow
- Update CI workflows and configurations, remove demo README and Fly configuration
- Update README, bump turbo version in package.json and pnpm-lock.yaml, and fix turbo schema URL
- Release 0.5.54 (#2306)

## [0.5.53] - 2026-05-11

### 🚀 Features

- Airhouse sa integration (#2274)
- Integrate Oxygen design system token layer (#2157)
- *(builder)* On-demand reference cards + onboarding quality + perf (#2234)
- Enhance comment formatting rules for Claude Code Review

### ⚙️ Miscellaneous Tasks

- Remove daily edge build schedule and adjust concurrency settings in public release workflow
- Weekly auto-fix 2026-05-10 (#2304)
- Update workspace exclude list to improve bundle size
- Release 0.5.53 (#2293)

## [0.5.52] - 2026-05-07

### 🚀 Features

- Airhouse onboarding (#2219)
- Billing allow tiered prices in provisioning (#2263)
- Enhance version bump script to read workspace member versions from Cargo.toml
- Update content changelog workflow to handle existing drafts and streamline PR creation
- Unified settings dialog (#2270)

### 🐛 Bug Fixes

- Web-app redirect immediately when deleting current workspace (#2271)

### 💼 Other

- *(deps)* Bump slackapi/slack-github-action from 3.0.2 to 3.0.3 (#2277)
- *(deps)* Bump fpicalausa/remove-stale-branches from 2.4.0 to 2.6.1 (#2276)
- *(deps-dev)* Bump lint-staged from 16.4.0 to 17.0.2 in the dev-npm-major-dependencies group across 1 directory (#2283)
- *(deps)* Bump the prod-npm-minor-dependencies group across 2 directories with 11 updates (#2279)

### ⚙️ Miscellaneous Tasks

- Update dependencies in TypeScript SDK and web app
- Release 0.5.52 (#2272)

## [0.5.51] - 2026-05-06

### 🚀 Features

- New demo_project, split demo_project into simple init template + internal_demo showcase (#2264)
- Admin endpoint to resync org billing from Stripe (#2258)
- Builder fixes and improvements (#2259)

### 🐛 Bug Fixes

- Update checkout step to use sparse checkout for Dockerfile

### ⚙️ Miscellaneous Tasks

- Update README banner (#2269)
- Release 0.5.51 (#2266)

## [0.5.50] - 2026-05-05

### 🚀 Features

- Add script to mirror 'oxy' container packages to 'oxygen' aliases
- Stripe integration (#2166)
- Improve mention (#2200)

### 🐛 Bug Fixes

- Retry transient OpenAI errors on OSS path and connection failures (#2260)
- Remove confusing "Local mode" banner (#2261)
- Web-app onboarding picker icon overflow + App2 dashboard not surfaced (#2262)

### ⚙️ Miscellaneous Tasks

- Update references from 'oxy' to 'oxygen' in workflows and scripts
- Update references from 'oxy-nightly' to 'oxygen-nightly' in public release workflow
- Release 0.5.50 (#2257)

## [0.5.49] - 2026-05-04

### ⚙️ Miscellaneous Tasks

- Update 'oxy' to 'oxygen' (#2255)
- Update repository references from 'oxy' to 'oxygen' in workflow files
- Filter Slack block types to prevent injection attacks (#2245)
- Release 0.5.49 (#2256)
- Update repository references from 'oxy' to 'oxygen' in public release workflow

## [0.5.48] - 2026-05-04

### 🚀 Features

- Use blockit to render sql aritfact (#2216)
- Implement verfied queries (#2229)

### 🐛 Bug Fixes

- Web-app reset AppPreview state when switching between apps (#2227)

### 🚜 Refactor

- Replace println! and styled text with structured tracing logs (#2226)
- DuckDB init_ducklake to use spawn_blocking (#2248)

### ⚡ Performance

- Optimize workflow state writes with incremental result deltas (#2228)

### 🧪 Testing

- Add unit tests for RetryContext advance methods (#2247)
- Add concurrent init race condition test for DuckDBPool (#2252)

### ⚙️ Miscellaneous Tasks

- Prevent CI jobs from running on release chore commits
- Weekly auto-fix 2026-05-03 (#2238)
- Add logging for DuckDB pool file-stat failures (#2243)
- Replace debug_assert with error handling for decider result validation (#2241)
- Refactor SQL placeholder generation into reusable helper (#2242)
- Escape SQL strings in DuckDB configuration (#2244)
- Add tests for Anthropic tool message conversion (#2246)
- Release 0.5.48 (#2224)

## [0.5.47] - 2026-05-01

### 🐛 Bug Fixes

- Home page false LLM key warning when secret is in env vars (#2218)

### ⚙️ Miscellaneous Tasks

- Release 0.5.47 (#2222)

## [0.5.46] - 2026-04-30

### 🚀 Features

- *(slack)* Add staging manifest for Oxygen Slack app
- Airform integration (#2089)

### 🐛 Bug Fixes

- Better handling for rate limit and tool error (#2207)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.45.1 to 1.45.2 (#2214)
- *(deps)* Bump slackapi/slack-github-action from 3.0.1 to 3.0.2 (#2213)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 6 updates (#2215)

### 🚜 Refactor

- Workspace manager and remove chart image storage (s3) (#2217)

### 📚 Documentation

- Airform docs (#2211)

### ⚙️ Miscellaneous Tasks

- Release 0.5.46 (#2209)

## [0.5.45] - 2026-04-29

### 🚀 Features

- Universal slack bot with multiple tenants support (#2134)
- *(s3)* Default to presigned URLs; drop ACL escape hatch (#2201)

### 🐛 Bug Fixes

- Remove unused import of EntityTrait in Slack tests
- Add back some old migrations instead of removing them
- *(slack)* Wire headless chart renderer so S3 env vars actually take effect
- *(slack)* Bot goes silent when chart-publisher init fails (#2205)
- Unify workspace onboarding for demo / github / blank (#2202)

### ⚙️ Miscellaneous Tasks

- Release 0.5.45 (#2199)

## [0.5.44] - 2026-04-28

### ⚙️ Miscellaneous Tasks

- Rebrand "Oxy" to "Oxygen" across codebase (#2194)
- Release 0.5.44 (#2198)

## [0.5.43] - 2026-04-28

### 🐛 Bug Fixes

- Issues with org switching / onboarding state leakage (#2188)
- Scope secret update/delete to project_id (#2192)

### ⚙️ Miscellaneous Tasks

- Release 0.5.43 (#2193)

## [0.5.42] - 2026-04-28

### 🐛 Bug Fixes

- Azure openai not working with new agentic workflow (#2189)

### ⚙️ Miscellaneous Tasks

- Release 0.5.42 (#2190)

## [0.5.41] - 2026-04-27

### 🐛 Bug Fixes

- Replace arduino/setup-protoc with apt for Protoc installation to avoid GitHub rate limits

### ⚙️ Miscellaneous Tasks

- Release 0.5.41 (#2187)

## [0.5.40] - 2026-04-27

### 🐛 Bug Fixes

- Update schema reference in consistency procedure YAML

### 💼 Other

- *(deps)* Upgrade arrow/parquet/duckdb to 58, sqlparser to 0.61 (#2159)

### 🚜 Refactor

- Web-app drop oxy gif from home page and tidy layout

### ⚙️ Miscellaneous Tasks

- Remove outdated arrow dependency version from Cargo.lock
- Release 0.5.40 (#2185)

## [0.5.39] - 2026-04-27

### 🚀 Features

- Polish agentic onboarding - live app titles, one-shot route (#2168)
- Airhouse integration (#2131)
- *(agentic-llm)* Enable Anthropic prompt caching for analytics + builder (#2172)
- Improve DuckDB onboarding upload UX (#2180)
- Validate LLM API key early in onboarding (blank + GitHub flows) (#2182)

### 🐛 Bug Fixes

- Adjust airlayer::View struct to new types (#2173)
- *(clickhouse)* Semantic query stats, host URL handling, and onboarding form (#2175)
- Harden agentic onboarding — drop unsupported warehouses, BigQuery key textarea, DuckDB stats resilience (#2178)
- Scope workspace name uniqueness per organization (#2183)

### 💼 Other

- Bump airlayer to latest main (b8c734d) (#2170)
- *(deps)* Bump the prod-npm-minor-dependencies group across 2 directories with 7 updates (#2155)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 2 directories with 2 updates (#2141)

### 🚜 Refactor

- Remove semantics.yml and oxy_globals (#2171)

### 📚 Documentation

- Update product-context.md from recent changelogs (#2184)

### ⚙️ Miscellaneous Tasks

- Update changelog action token handling
- Weekly auto-fix 2026-04-26 (#2177)
- Update .trivyignore comments for clarity and add missing file path
- Update json schemas
- Update GitHub App token configuration to use client-id instead of app-id
- Release 0.5.39 (#2167)

## [0.5.38] - 2026-04-24

### 🚀 Features

- Support authentication with rds using aws iam role auth (#2135)
- Agentic onboarding (#1850)

### 🐛 Bug Fixes

- Permission bugs (#2146)

### 💼 Other

- Add Trivy security scanning to CI/CD pipeline (#2156)

### ⚙️ Miscellaneous Tasks

- Fix multiple ci issues
- Add CSS and JS file types to workflow paths (#2160)
- Release 0.5.38 (#2145)

## [0.5.37] - 2026-04-23

### 🚀 Features

- Add direct pull functionality to IDE header when on main  (#2096)
- Duckdb/postgres/clickhouse  observability backend (#2094)
- Support multi tenancy (#2040)
- Apply facade to agentic crates (#2034)
- Multi org improvement (#2120)

### 🐛 Bug Fixes

- Add id-token write permission and remove invalid input in content-changelog workflow (#2092)
- Update bump-version script to modify Cargo.lock along with Cargo.toml
- Agentic.yml validation, runtime errors, secrets resolution, Snowflake stats (#2102)
- *(web-app)* Chart/answer gap + sidebar icon alignment (#2108)
- Add retry logic for transient PostgreSQL connection errors (#2109)
- Hotfix for backend selection observability and add docs (#2113)
- Update dependencies rand for security issue
- *(duckdb)* Validate path, filter extensions, handle collisions (#2122)
- *(server)* Install PlatformContext and BuilderBridges in local mode (#2130)
- Agentic state init (#2133)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 8 updates (#2116)
- *(deps)* Bump astral-sh/setup-uv from 5 to 7 (#2115)
- *(deps)* Bump lewagon/wait-on-check-action from 1.6.1 to 1.7.0 (#2114)
- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 9 updates (#2118)
- *(deps)* Bump the prod-cargo-major-dependencies group across 1 directory with 17 updates (#2121)
- Upgrade rust toolchains
- Bump arrow to 57, upgrade lancedb/duckdb/snowflake-api (#2129)
- *(deps)* Bump actions/github-script from 8 to 9 (#2125)
- *(deps)* Bump peter-evans/create-pull-request from 7 to 8 (#2124)
- *(deps)* Bump rustls from 0.23.38 to 0.23.39 in the prod-cargo-minor-dependencies group across 1 directory (#2138)

### 🚜 Refactor

- *(docs)* Update backend architecture link and add new documentation file

### 📚 Documentation

- Update product-context.md from recent changelogs (#2110)

### ⚙️ Miscellaneous Tasks

- Update changelog workflow to support manual PR resolution and refine version input description
- Auto update product context (#2099)
- Weekly auto-fix 2026-04-19 (#2106)
- Format CI workflow and remove clickhouse dependency
- Upgrade react-syntax-highlighter to v16 and fix refractor imports (#2123)
- Update pnpm and action-gh-release versions in CI workflows
- Enhance Slack notification conditions for stable releases and changelog updates
- Add verification for edge Docker image publication in Slack announcements
- Update npm package ecosystem to include web-app directory for dependency updates
- Downgrade windows-sys and socket2 dependencies in Cargo.lock
- Update cargo deps
- Update dependencies in package.json
- Downgrade pnpm action version to v5
- Release 0.5.37 (#2093)

## [0.5.36] - 2026-04-16

### 🚀 Features

- Builder agent improvements (#2078)
- Add time awareness to analytics agent (#2082)
- Allow save on main for local proj and deploy to separate branch (#2087)
- Add verified badge on analytic agent (#2081)

### 🐛 Bug Fixes

- Handle batched tool_use blocks when resuming from suspension (#2054)
- Shutdown signal handling (#2066)
- Markdown directives swallow colons in text (e.g. timestamps) (#2083)
- Task-level replay button now triggers workflow execution from selected step (#2077)
- Resolve duplicate error display and add dismiss button in SQL editor (#1809)

### 💼 Other

- *(deps)* Bump actions/upload-artifact from 4 to 7 (#2067)
- *(deps)* Bump the prod-npm-minor-dependencies group with 11 updates (#2069)
- *(deps)* Bump crate-ci/typos from 1.44.0 to 1.45.1 (#2068)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 8 updates (#2071)
- *(deps)* Upgrade vite from 7.3.1 to 8.0.8 in package.json and pnpm-lock.yaml
- *(ci)* Simplify public announce workflow and enhance changelog generation
- *(deps-dev)* Bump typescript from 5.9.3 to 6.0.2 in the dev-npm-major-dependencies group across 1 directory (#2080)

### 🚜 Refactor

- Simplify conditionals in extract_all_propose_changes and validation functions

### 📚 Documentation

- Delete internal-docs/local-git-branching.md

### ⚙️ Miscellaneous Tasks

- Update prepare-release workflow to install uv and enhance changelog generation script
- Refactor changelog generation workflow and update dependencies to version 0.5.35
- Upgrade airlayer to v0.0.9 (#2053)
- Use git-cliff --latest for GitHub Release notes on public repo
- Unify all deps and optimize ci (#2063)
- Remove automated label from weekly enhancement issue creation
- Update version bump rules for pre-1.0 and post-1.0 handling
- Release 0.5.36 (#2062)

## [0.5.35] - 2026-04-10

### 🐛 Bug Fixes

- Use produced key for hotkeys to support all keyboard layouts (#2056)

### ⚙️ Miscellaneous Tasks

- Update release scripts to use uv and add content changelog generation
- Enhance changelog generation with local dry-run tool and improved context fetching
- Update conditions for changesets and claude-review jobs to exclude release branches
- Release 0.5.35 (#2061)

## [0.5.34] - 2026-04-10

### 🐛 Bug Fixes

- Builder agent config variants (#2043)

### 💼 Other

- Switch to release-plz from google release please (#2057)

### ⚙️ Miscellaneous Tasks

- Refactor release process and remove release-plz configuration
- Release 0.5.34 (#2058)
- Release 0.5.34 (#2059)
- Extract version from commit message using regex pattern
- Release 0.5.34 (#2060)

## [0.5.33] - 2026-04-09

### 🚀 Features

- Thinking budget toggle (#1847)
- New builder agent (#1815)
- Workspace management and onboarding process (#1821)
- Handle multi installations and multi accounts
- Add GitHub access token handling for user authentication and installation management

### 🐛 Bug Fixes

- Ui ux semantic inconsistent (#1770)
- Builder bugs (#2036)
- Context graph UI (#2037)
- Enhance branch naming and UI adjustments in BranchQuickSwitcher
- Enhance base URL extraction for GitHub OAuth flow
- Improve GitHub OAuth flow and installation handling
- Implement selection token for GitHub OAuth installation picking flow
- Github multi account installation
- Upgrade major versions for frontend packages (#1839)
- Bring back DuckDB initialization logic and state management
- Update dependency consolidation steps to avoid running cargo commands
- Add critical note on handling default features for workspace dependencies
- Update dependencies to latest versions for improved stability and performance

### 💼 Other

- *(deps)* Bump actions/create-github-app-token from 2 to 3 (#2031)
- *(deps)* Bump lewagon/wait-on-check-action from 1.5.0 to 1.6.1 (#2030)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 10 updates (#2035)

### 🚜 Refactor

- Scan entire project for semantic layer files (#1843)
- Initialization guards, tool descriptions, and error handling (#1853)

### 📚 Documentation

- Update workspace layout in CLAUDE.md and workflow file for clarity and completeness
- Add docs for the new cloud mode and local mode
- Update GitHub App setup documentation for clarity and accuracy
- Add docs for the new cloud mode and local mode (#2042)

### ⚙️ Miscellaneous Tasks

- Upgrade airlayer to latest commit (#1848)
- Add weekly enhancements workflow for Claude
- Add missing GITHUB_TOKEN environment variable in workflow
- Add weekly auto-fix workflow for Rust and frontend formatting
- Remove version specification for pnpm installation
- Enhance weekly auto-fix workflow with Rust toolchain and pnpm cache setup
- Update weekly auto-fix workflow to use GitHub App token for authentication
- Enhance weekly auto-fix workflow with dynamic date in commit message and PR title
- Update autofix workflow
- Update weekly autofix workflow configuration
- Weekly auto-fix 2026-04-08 (#2041)
- Weekly auto-fix 2026-04-09 (#2046)
- Add transform configuration for handling import.meta in TypeScript SDK
- Update dependencies in package.json for sdk/typescript and web-app
- Update dependencies across multiple crates to latest versions
- Narrow down duckdb version
- Enhance weekly workflows with dependency consolidation and analysis steps
- Enable full output for Claude code review and weekly enhancements workflows
- Disable full output for Claude code review in workflows
- *(main)* Release (#1845)

## [0.5.32] - 2026-04-02

### 🚀 Features

- New fsm agentic workflow (#1781)

### 🐛 Bug Fixes

- Issue with testing progress bar (#1827)
- Biome format (#1840)

### 💼 Other

- *(deps)* Bump slackapi/slack-github-action from 2.1.1 to 3.0.1 (#1829)

### ⚙️ Miscellaneous Tasks

- Unify rust version, update rust and format codes
- Add version to agentic core
- *(main)* Release (#1817)

## [0.5.31] - 2026-03-26

### 🚜 Refactor

- Update control type in TimeDimensionsField to WorkflowFormData and improve type safety

### ⚙️ Miscellaneous Tasks

- Release 0.5.31
- *(main)* Release (#1814)

## [0.5.30] - 2026-03-26

### 🚀 Features

- Replace CubeJS with o3 for in-process semantic SQL compilation (#1794)
- Add secrets management feature with environment variable support (#1795)
- Detect secrets from ENV (#1812)
- Git workflow for IDE (#1790)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-major-dependencies group across 1 directory with 2 updates (#1803)

### 🚜 Refactor

- Remove rollup-plugin-esbuild and update visualizer configuration
- Simplify conditional statements for clarity in multiple files
- Remove Cube.js semantic engine and update related documentation (#1810)

### ⚙️ Miscellaneous Tasks

- Upgrade depes
- Add peer dependency rules for TypeScript ESLint packages in package.json
- *(main)* Release (#1808)

## [0.5.29] - 2026-03-24

### 🐛 Bug Fixes

- Chart rendering race condition
- Chart rendering race condition (#1800)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 13 updates (#1805)
- *(deps)* Bump dorny/paths-filter from 3 to 4 (#1802)
- *(deps)* Bump pnpm/action-setup from 4 to 5 (#1801)

### ⚙️ Miscellaneous Tasks

- Release 0.5.29
- *(main)* Release (#1798)

## [0.5.28] - 2026-03-23

### 🚀 Features

- Add Makefile for project setup, build, and development tasks
- Support Looker Explore in Dev Portal semantic layer (#1730)
- Remove database dependency for oxy run cli command  (#1778)
- New testing UI (#1727)
- Add GitHub Copilot code review instructions
- Allow for individual test case runs via cli (#1792)
- Add lovable integration instructions and starter prompt to our public docs (#1760)

### 🐛 Bug Fixes

- Change log level to debug for checkpoint not found error (#1749)

### 💼 Other

- *(deps-dev)* Bump tsdown from 0.20.3 to 0.21.2 in the dev-npm-minor-dependencies group across 1 directory (#1767)
- *(deps)* Bump docker/build-push-action from 6 to 7 (#1786)
- *(deps)* Bump docker/setup-buildx-action from 3 to 4 (#1785)

### 📚 Documentation

- Add product context to claude (#1784)
- Update welcome documentation for clarity and precision

### ⚙️ Miscellaneous Tasks

- Format codes
- Update dependency to resolve dependabot issue
- *(main)* Release (#1779)

## [0.5.27] - 2026-03-12

### 🐛 Bug Fixes

- Db connections not reused (#1763)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1776)

## [0.5.26] - 2026-03-12

### 🚀 Features

- Enhance DateControl with calendar popover and input synchronization
- Replace ontology with context graph (#1771)

### 🐛 Bug Fixes

- Remove unnecessary react-dom/client dependency from Vite config
- Optimize chunking strategy in Vite config for better performance

### 📚 Documentation

- Clarify Oxygen platform description (#1769)

### ⚙️ Miscellaneous Tasks

- Update dependencies in TypeScript SDK and web app
- *(main)* Release (#1764)
- Release 0.5.26
- *(main)* Release (#1772)

## [0.5.25] - 2026-03-11

### 🚀 Features

- Agent testing framework (#1714)
- Improve chart (#1728)
- Add control and row to block editor of apps (#1761)

### 🐛 Bug Fixes

- Always log to file, mirror to stdout when OXY_DEBUG=true (#1759)
- UI ux inconsitant (#1740)
- Skip hidden dirs in agent/workflow file scanning (#1762)

### 💼 Other

- *(deps)* Bump docker/login-action from 3 to 4 (#1752)
- *(deps)* Bump crate-ci/typos from 1.43.5 to 1.44.0 (#1751)

### ⚙️ Miscellaneous Tasks

- Update json schemas
- *(main)* Release (#1756)

## [0.5.24] - 2026-03-09

### 🚀 Features

- Add type to app result task outputs (#1731)

### 🐛 Bug Fixes

- Update email validation logic and configuration for magic link authentication

### 📚 Documentation

- Update deployment docs

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1748)

## [0.5.23] - 2026-03-07

### 🐛 Bug Fixes

- Infinite loop on FE for procedure with loops (#1746)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1747)

## [0.5.22] - 2026-03-06

### 🚀 Features

- Improve looker task form (#1729)

### 🐛 Bug Fixes

- Disable "Automate this" button in readonly mode (#1739)
- Run index duplication (#1743)

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group across 1 directory with 21 updates (#1741)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1742)

## [0.5.21] - 2026-03-06

### 🚀 Features

- Update permissions in Claude review workflow to include actions read access
- Login by magic link with email (#1713)
- Rename automation to procedure (#1715)
- Add simple invitation flow for magic login (#1717)
- Add API Keys page and integrate into settings layout (#1724)
- Looker integration (#1542)

### 🐛 Bug Fixes

- Add horizontal padding to thread message containers (#1711)
- Remove built-in mode references and clean up authentication logic (#1716)
- Enhance magic link email design and styling
- Add bottom padding to FieldsSelectionPanel for full scroll (#1709)
- Update logo image source in invitation and magic link email templates
- Simplify logo display in invitation and magic link email templates
- Update logo dimensions in invitation and magic link email templates
- Update footer copyright and description in invitation and magic link email templates

### 💼 Other

- *(deps)* Bump actions/upload-artifact from 6 to 7 (#1719)
- *(deps)* Bump actions/download-artifact from 7 to 8 (#1720)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 6 updates (#1725)
- *(deps-dev)* Bump rollup-plugin-visualizer from 6.0.5 to 7.0.0 in the dev-npm-major-dependencies group (#1722)

### 📚 Documentation

- Update welcome doc
- Add WSL 2 setup and troubleshooting for Rancher Desktop on Windows
- Streamline getting-started flow and add prerequisites page

### ⚙️ Miscellaneous Tasks

- Refactor code for improved readability and performance in various modules
- Release 0.5.21
- Comment out free disk space step in CI and coverage workflows
- Remove image from email signin
- *(main)* Release (#1712)

## [0.5.20] - 2026-02-27

### 🚀 Features

- Update async-openai dependency to version 0.33.0 and adjust features across crates
- Refactor automations into routines (#1686)
- Enhance semantic_query output for chaining (#1694)

### 📚 Documentation

- Rename Oxy to Oxygen throughout README

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1704)

## [0.5.19] - 2026-02-26

### 🚀 Features

- Add readonly mode support to server and API endpoints (#1700)
- Enhance anthropic support  (#1701)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.43.4 to 1.43.5 (#1678)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 10 updates (#1696)

### 📚 Documentation

- Add readonly mode

### ⚙️ Miscellaneous Tasks

- Add Claude Code Review workflow for automated pull request reviews
- Add missing id-token permission for Claude Code Review workflow
- Enhance Claude Code Review workflow with detailed prompt and tracking
- Update Claude Code Review workflow to trigger on reopened pull requests
- Smoke test not running the right sql
- Gen config schema
- *(main)* Release (#1684)

## [0.5.18] - 2026-02-18

### 🚀 Features

- Rename Local Development Project to Oxygen and Dev Mode to Developer Portal (#1682)
- Cache app result by default (#1675)

### 🐛 Bug Fixes

- Use plural path for apps (#1683)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1677)

## [0.5.17] - 2026-02-15

### 🚀 Features

- Add DOMO_DEVELOPER_TOKEN to CI environment variables
- Add DOMO_DEVELOPER_TOKEN to CI environment variables

### 🐛 Bug Fixes

- Update method name for retrieving charts directory in get_app_result (#1674)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1673)

## [0.5.16] - 2026-02-13

### 🚀 Features

- Add domo support (#1665)
- Add internal API server support with authentication middleware
- Add internal API server support with authentication bypass (#1667)
- Export viz by api - use serverside rendering (#1668)
- Add headless browser support for server-side ECharts rendering … (#1670)
- App api (#1671)

### 🐛 Bug Fixes

- Update coverage report command to specify profile
- Agent problem with time dimensions (#1659)
- Show time dimension in x axis dropdown (#1658)
- Internal port (#1672)
- Add missing dependencies for headless browser support in Dockerfiles
- Update Chromium path in Dockerfiles and remove unused dependencies
- Add another simpler fix for headless chrome

### 💼 Other

- *(deps-dev)* Bump eslint from 9.39.2 to 10.0.0 in the dev-npm-major-dependencies group (#1637)
- Simplify Vite cache path in public release workflow
- Add job to download latest edge binary for Dockerfile-only changes
- Remove caching from release CLI job in public release workflow

### ⚙️ Miscellaneous Tasks

- Update CI configuration with new API keys and change working directory for smoke tests
- Update workflows to include docker outputs in changesets and coverage jobs
- Update permissions for Changesets job in coverage workflow
- *(main)* Release (#1669)

## [0.5.15] - 2026-02-12

### 🚀 Features

- Implement incremental build system for semantic layer (#1652)

### 🐛 Bug Fixes

- Update actions/download-artifact version to v7 in CI and release workflows
- Update health endpoint URLs and enhance smoke tests with additional API checks
- Update coverage workflow and improve dev build performance

### 💼 Other

- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 7 updates (#1663)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 6 updates (#1634)
- *(deps)* Bump the prod-npm-minor-dependencies group with 6 updates (#1633)
- Fix frontend build issue
- Improve frontend build pipeline

### ⚙️ Miscellaneous Tasks

- Update lint-staged configuration to prevent errors on unmatched files
- Update link text from "Dev Mode" to "Dev Portal" in sidebar and tests
- Update deps
- Update sccache configuration for clarity and maintainability
- *(main)* Release (#1666)

## [0.5.14] - 2026-02-11

### 🐛 Bug Fixes

- Resolve .semantics permission issues on WSL (#1661)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1660)

## [0.5.13] - 2026-02-11

### 🚀 Features

- Add version banner (#1656)

### 🐛 Bug Fixes

- Special characters are causing the frontend to crash (#1654)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1657)

## [0.5.12] - 2026-02-11

### 🐛 Bug Fixes

- Ensure Slack notification is sent only when a channel is specified
- Remove unnecessary package build step in CI workflow
- Otel not save traces (#1653)

### ⚙️ Miscellaneous Tasks

- Update release scripts to improve timestamp handling and concurrency management
- Split coverage workflow to reduce time
- Update Slack notification channel to product-releases-prod
- *(main)* Release (#1655)

## [0.5.11] - 2026-02-10

### 🚀 Features

- Implement incremental build system for semantic layer (#1649)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.43.0 to 1.43.4 (#1632)

### ⚙️ Miscellaneous Tasks

- Release 0.5.11
- *(main)* Release (#1650)

## [0.5.10] - 2026-02-10

### 🐛 Bug Fixes

- App runner task output should be accumulate (#1646)

### 🧪 Testing

- Update test command to limit failures and improve coverage reporting

### ⚙️ Miscellaneous Tasks

- Update installation script
- App builder semantic query filter improvements (#1647)
- Update CI workflow to remove max-fail option
- *(main)* Release (#1648)

## [0.5.9] - 2026-02-10

### 🐛 Bug Fixes

- Cannot connection otel (#1645)
- Allow proper injection of task output to agent prompt (#1644)
- Missing default tool name (#1642)

### 📚 Documentation

- Update README and script to reflect changes from nightly to edge builds

### ⚙️ Miscellaneous Tasks

- Update CI workflow
- *(main)* Release (#1643)

## [0.5.8] - 2026-02-10

### 🚀 Features

- Time dimension (#1627)
- Add script to list available Oxy releases with filtering options

### 🐛 Bug Fixes

- Ensure jobs run only on successful dependencies in public release workflow
- Add markdown specifier for table data (#1640)

### 💼 Other

- *(deps)* Bump actions/checkout from 4 to 6 (#1631)

### 🚜 Refactor

- Simplify variable usage and improve readability in CI and auth modules
- Consolidate E2E tests into a single workflow

### ⚙️ Miscellaneous Tasks

- Add json schemas
- Shorten readme
- *(main)* Release (#1641)

## [0.5.7] - 2026-02-09

### 🚀 Features

- Improve sql ide (#1609)
- Add manual refresh database sidebar (#1619)

### 🐛 Bug Fixes

- Use workflow_ref for workflow execution (#1616)
- Remove double routing agent name prefix (#1617)
- App crash when run workflow (#1620)
- Handle unique constraint violation when creating user (#1621)
- Remove tracing logs and improve database management options (#1622)
- Show semantic queries in automation run logs (#1613)
- Sometimes save change does not appear (#1625)
- Enhance enterprise network removal with retry logic (#1626)
- Sometimes save change does not appear (#1628)
- Update PostgreSQL image from 18-alpine to 18.1 across configurations
- Artifact not save (#1630)

### 🧪 Testing

- Add test coverage and enhance unit test (#1598)
- Add smoke test
- Simplify cargo build command in CI workflow

### ⚙️ Miscellaneous Tasks

- Restructure GitHub workflows for improved release management and Docker image publishing
- *(main)* Release (#1618)

## [0.5.6] - 2026-02-05

### 🚀 Features

- *(logging)* Enhance logging configuration for cloud and local environments (#1604)
- Add strict validation for yaml objects (#1603)

### 🐛 Bug Fixes

- Semantic query execution error persist (#1599)
- Enhance semantic container management with volume mount validation (#1602)
- Timezone in query result (#1610)

### 💼 Other

- *(deps-dev)* Bump markdownlint-cli2 from 0.17.2 to 0.20.0 in the dev-npm-minor-dependencies group across 1 directory (#1607)

### 🚜 Refactor

- Use a better linter & formatter for web (#1605)

### 🧪 Testing

- Update typos configuration to include consts and fix formatting

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1597)
- Update dependencies in TypeScript SDK and web app
- Release 0.5.6
- *(main)* Release (#1611)

## [0.5.5] - 2026-02-03

### 🚀 Features

- Optimize docker buildtime (#1582)
- Sql ide (#1556)
- Add execute sql query shortcut (#1591)
- Update workflows for Slack notifications and concurrency management

### 🐛 Bug Fixes

- Result query table state reset (#1590)
- Add paging and sort for sql result table (#1594)

### 💼 Other

- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 4 updates (#1573)
- Disable provenance in Docker build args
- *(deps)* Bump crate-ci/typos from 1.42.2 to 1.43.0 (#1592)

### ⚙️ Miscellaneous Tasks

- Update deps
- Update tag for edge build
- Upgrade rust version (#1589)
- *(main)* Release (#1588)

## [0.5.4] - 2026-01-30

### 🐛 Bug Fixes

- Update StartArgs to include enterprise flag in ServeArgs

### 🧪 Testing

- Prevent parallel test with build

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1581)

## [0.5.3] - 2026-01-30

### 🐛 Bug Fixes

- Bring back enterprise flag for oxy serve

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1579)

## [0.5.2] - 2026-01-30

### 🚀 Features

- Add better check for better runtime and clarify in docs (#1577)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1578)

## [0.5.1] - 2026-01-29

### 🚜 Refactor

- Simplify output handling and improve readability in various modules

### ⚙️ Miscellaneous Tasks

- Release 0.5.0
- *(main)* Release (#1574)
- Bump version
- Release 0.5.1
- *(main)* Release (#1576)

## [0.4.11] - 2026-01-29

### 🚀 Features

- Move database management and activity logs to the developer portal (#1550)
- Add oxy start enterprise (#1551)
- Add document for observability (#1553)
- Remove sqlite (#1557)
- Add clean option to start command and improve Docker management (#1568)
- Start semantic engine in Oxy Enterprise (#1571)

### 🐛 Bug Fixes

- Observability follow up (#1555)
- Semantic query artifact (#1566)
- Oxy build cannot run (#1572)

### 💼 Other

- *(deps)* Bump lewagon/wait-on-check-action from 1.4.1 to 1.5.0 (#1560)
- *(deps)* Bump crate-ci/typos from 1.42.1 to 1.42.2 (#1559)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 9 updates (#1561)

### 🚜 Refactor

- Reduce number of logs (#1567)

### 📚 Documentation

- Re-add docs folder into oxy internal (#1558)
- Remove some non existent folder
- Oxy start with --clean

### 🧪 Testing

- Handle some edge cases in tests

### ⚙️ Miscellaneous Tasks

- Update release version output to use correct variable format
- Update docs
- Add back apple intel to installation script
- Minor improvements
- *(main)* Release (#1552)

## [0.4.10] - 2026-01-22

### 🚀 Features

- Implement observability metrics (#1521)
- Add file input option for intent testing and improve question retrieval (#1540)
- #1537 make various fields in the GUI dropdowns / search filters (#1543)
- Create an add file button in object view in dev portal (#1541)
- Llm infra slice (#1526)

### 🐛 Bug Fixes

- Allow permissive deserialization of openai completion responses (#1539)
- V0 merge conflicts (#1548)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.42.0 to 1.42.1 (#1530)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 2 updates (#1532)
- *(deps-dev)* Bump the dev-npm-major-dependencies group with 5 updates (#1534)

### 🧪 Testing

- Fix end to end tests failure

### ⚙️ Miscellaneous Tasks

- Add missing sqlite to postgres file
- Add mode switcher for topic and view IDE + error handling + code refactoring (#1482)
- Cargo format and fix
- Update release pls config
- Update release version output to use correct variable from release-please
- *(main)* Release (#1538)

## [0.4.9] - 2026-01-19

### 🚀 Features

- Upgrade deps

### 💼 Other

- Enable intel build again for mac

### ⚙️ Miscellaneous Tasks

- Correct output keys for release version in prepare-release workflow
- *(main)* Release (#1527)

## [0.4.8] - 2026-01-16

### 🐛 Bug Fixes

- Data app not rendering (#1524)

### ⚙️ Miscellaneous Tasks

- Update dependencies and improve default implementations
- Update e2e tests config to not use http2 anymore
- *(main)* Release (#1525)

## [0.4.7] - 2026-01-15

### 🚀 Features

- Optimize query result table for large database (#1501)
- Closes #1409 research spike ide vibe coding (#1499)
- Implement observability (#1424)

### 🐛 Bug Fixes

- Upgrade deps

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.41.0 to 1.42.0 (#1510)

### 🚜 Refactor

- Slices  (#1493)

### ⚙️ Miscellaneous Tasks

- Adjust timeout settings for E2E tests in workflow
- Update some supporting components
- Release 0.4.7
- *(main)* Release (#1514)

## [0.4.6] - 2026-01-12

### 🐛 Bug Fixes

- Downgrade to bookworm to fix compatability with cube

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 4 updates (#1485)

### ⚙️ Miscellaneous Tasks

- Release 0.4.6
- *(main)* Release (#1503)

## [0.4.5] - 2026-01-08

### 🚀 Features

- Support sorting in semantic explorer (#1466)
- Recording user triggering automation run (#1460)

### 🐛 Bug Fixes

- Boolean handling for semantic filters (#1480)
- Enable x86_64 linux debug builds and update cargo commands to use release profile
- Semantic query artifact (#1494)
- Sqlite migration causing foreign key constraint issue  (#1498)

### 💼 Other

- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 2 updates (#1471)
- *(deps-dev)* Bump typescript-eslint from 8.50.1 to 8.51.0 in the dev-npm-minor-dependencies group (#1469)
- Switch target to release
- *(deps-dev)* Bump globals from 16.5.0 to 17.0.0 in the dev-npm-major-dependencies group (#1487)
- *(deps)* Bump crate-ci/typos from 1.40.1 to 1.41.0 (#1483)
- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 6 updates (#1488)

### 🎨 Styling

- Typos multilingual

### ⚙️ Miscellaneous Tasks

- Remove cache all crates
- Temporarily disable x86_64 linux debug builds in public nightly workflow
- Remove python support (#1490)
- Add back sqlx sqlite to sea orm migration
- *(main)* Release (#1481)

## [0.4.4] - 2026-01-01

### 🚀 Features

- Implement delete workflow run (#1476)
- Deploy to flyio (#1459)

### 🐛 Bug Fixes

- Line chart (#1464)
- Upgrade docker options for bollard
- State dir should be inferred from env (#1474)
- Chart crashing app (#1475)
- Semantic layer sidebar scroll (#1477)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.40.0 to 1.40.1 (#1467)
- *(deps)* Bump the prod-cargo-major-dependencies group across 1 directory with 13 updates (#1472)

### 🚜 Refactor

- Error in db

### ⚙️ Miscellaneous Tasks

- Run cargo format and better manage deps (#1462)
- Update oxy docs link
- *(main)* Release (#1463)
- Release 0.4.3
- *(main)* Release (#1478)
- Bump version by hand and trigger release please again
- Release 0.4.4
- *(main)* Release (#1479)

## [0.4.1] - 2025-12-25

### 🚀 Features

- Integrate browserauth config (#1439)
- Kestra like ide (#1437)
- A2a support (#1426)

### 🐛 Bug Fixes

- Prevent path traversal attack (#1446)
- Improve left sidebar spacing and alignment (#1456)
- Bugs with filters in semantic panel and decimal not displayed correctly (#1458)

### 💼 Other

- *(deps)* Bump actions/download-artifact from 6 to 7 (#1450)
- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 6 updates (#1454)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 2 updates (#1452)

### 🧪 Testing

- Enhance e2e tests for chat and IDE
- Clarify comments and ensure Files view mode is set before tests

### ⚙️ Miscellaneous Tasks

- Remove docs to unify at oxy-content repo (#1445)
- Update min consistency
- Update deps
- Enhance artifact handling and validation in workflows
- Update packages
- Update build commands to use debug target for edge
- *(main)* Release (#1448)

## [0.4.0] - 2025-12-19

### 📚 Documentation

- Add migration guide and update oxy start doc

### ⚙️ Miscellaneous Tasks

- Remove duplicated build aw
- Release 0.4.0
- *(main)* Release (#1444)

## [0.3.21] - 2025-12-18

### 🚀 Features

- Rename workflows to automations and support .automation.yml ext… (#1435)
- Closes #1352 refactor the base agent into agentic workflows (#1407)
- Add semantic explorer to ide (#1423)
- Add app files to ontology view with semantic topic connections (#1441)
- Migrate from SQLite to PostgreSQL with new command `oxy start` (#1425)

### 🐛 Bug Fixes

- Arg TimeoutLayer::with_status_code

### 💼 Other

- *(deps)* Bump actions/upload-artifact from 5 to 6 (#1428)
- *(deps)* Bump actions/cache from 4 to 5 (#1427)
- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 2 updates (#1431)
- *(deps-dev)* Bump @types/node from 24.10.3 to 25.0.2 in the dev-npm-major-dependencies group (#1433)

### 🎨 Styling

- Cargo clippy

### ⚙️ Miscellaneous Tasks

- Remove darwin from build job
- Update rust version
- Upgrade deps
- Update timeout to use latest method from tower
- *(main)* Release (#1436)

## [0.3.20] - 2025-12-11

### 🚀 Features

- Enhance consistency checks with customizable prompts (#1403)
- Data app chart date handling (#1422) 
- Slack integration (#1373)

### 💼 Other

- *(deps)* Bump actions/upload-artifact from 4 to 5 (#1412)
- *(deps)* Bump crate-ci/typos from 1.39.2 to 1.40.0 (#1411)
- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 10 updates (#1414)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 10 updates (#1415)

### 📚 Documentation

- Make it clear that entities have to reference dimensions (#1406)

### ⚙️ Miscellaneous Tasks

- Update web deps
- *(main)* Release (#1418)

## [0.3.19] - 2025-12-04

### 🚀 Features

- Add nightly cleanup workflow to remove old releases and Docker tags
- Add customer demo project (#1399)
- Enhance testing capabilities with JSON output and accuracy thresholds (#1401)
- Heuristic chart render (#1397)

### 🐛 Bug Fixes

- Update CODEOWNERS email to reflect new domain
- Update SHORT_SHA retrieval method in workflow to use GITHUB_SHA
- Update permissions and simplify conditional expression in public-nightly-build workflow
- Tool choice error cause by async_openai (#1395)
- Update Playwright tests to wait for network idle state and increase navigation timeout
- Can not enter space in workflow IDE (#1404)

### 🚜 Refactor

- Update E2E tests and Slack notifications; refactor SQL queries for sales analysis
- Update workspace exclude patterns to prevent unnecessary rebuilds

### 📚 Documentation

- Enhance installation instructions for Oxy CLI with edge and nightly build options

### 🧪 Testing

- Add more end-to-end tests for IDE functionality, navigation, and threads listing
- Add more end-to-end tests for IDE functionality, navigation, and threads listing (#1396)
- Enhance E2E testing setup with global setup script and test agent file
- Add cleanup step after all navigation and threads listing tests
- Refactor E2E tests to use mocked thread data and improve setup efficiency
- Enhance E2E workflow to conditionally run tests based on changesets output
- Add permissions for actions and pull-requests in Changesets job
- Integrate consistency tests in end to end (#1402)
- Correct spelling of 'temperature' in SQL comments
- Adjust backend server startup and health check timeout in E2E tests

### ⚙️ Miscellaneous Tasks

- Update CI workflows for improved environment handling and Slack notifications
- Simplify conditional expression for build-channel job in public-nightly-build workflow
- Streamline conditional expression for Docker image build in public-nightly-build workflow
- Update runner configuration to use ubuntu-slim for specific repository
- Set runner environment to ubuntu-latest in Open Source workflow
- Enhance backend server readiness checks and log output on failure
- *(main)* Release (#1400)

## [0.3.18] - 2025-11-27

### 🚀 Features

- Closes #1298 spike gui editor v2 (#1323)
- Flexible mcp (#1378)
- Add a docker-compose that serves as demo entrypoint (#1390)

### 🐛 Bug Fixes

- Break empty workflow thread page (#1383)
- Navigate to thread page after send message in the home page (#1389)
- Can not run workflow thread (#1392)
- Restrict Slack notification to specific repository on main branch failure
- Update backend server startup command to build before running

### 💼 Other

- *(deps)* Bump actions/checkout from 5 to 6 (#1385)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 3 updates (#1384)

### 🚜 Refactor

- Remove PostgreSQL client installation and backend readiness checks from E2E workflow
- Update backend server startup process in E2E workflow

### 🧪 Testing

- Enhance e2e (#1393)
- Change directory before installing Playwright browsers
- Add wait step for backend server readiness in E2E workflow
- Fix Playwright test execution
- Update backend server startup command and log PID
- Enhance E2E workflow
- Update backend server startup command to use HTTP/2 only
- Reuse cache across jobs
- Update health check URL to use HTTPS for backend server readiness
- Change test results upload condition to only on failure
- Adjust E2E test runner configuration for resource optimization

### ⚙️ Miscellaneous Tasks

- Upgrade deps
- Upgrade deps
- Remove old e2e test for onboarding
- *(main)* Release (#1387)

## [0.3.17] - 2025-11-20

### 🚀 Features

- Add copy functionality and expand/collapse feature to workflow output components (#1379)
- Deep research (#1367)

### 🐛 Bug Fixes

- Sqlite transaction (#1368)
- Github app webhook (#1362)
- Closes #1319 workflow preview with error (#1377)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.39.0 to 1.39.2 (#1375)

### ⚙️ Miscellaneous Tasks

- Upgrade deps
- *(main)* Release (#1369)

## [0.3.16] - 2025-11-13

### 🚀 Features

- Unify semantics (#1213)
- Implement auto-expand for the last log item in OutputLogs (#1359)
- Add ScrollToBottomButton component and integrate with useSmartScroll hook
- Improve auto scrolling and add a scroll to latest (#1360)

### 🐛 Bug Fixes

- Ide save file bugs (#1355)
- Include file extension inside the duckdb schema sql (#1357)
- Sidebar bugs (#1356)

### 💼 Other

- *(deps-dev)* Bump typescript-eslint from 8.46.3 to 8.46.4 in the dev-npm-minor-dependencies group (#1354)

### 📚 Documentation

- Add sql_query inline option to execute_sql workflow type (#1363)

### 🎨 Styling

- Refactor code

### ⚙️ Miscellaneous Tasks

- Update dependencies
- *(main)* Release (#1350)

## [0.3.15] - 2025-11-09

### 🚀 Features

- Functioning v0 ontology graph (#1341)
- Snowflake row level security (#1327)

### 🐛 Bug Fixes

- Handle agent context objects with src arrays in ontology graph (#1344)
- Ide save file bugs (#1345)

### ⚙️ Miscellaneous Tasks

- Update authentication docs
- *(main)* Release (#1343)

## [0.3.14] - 2025-11-05

### 🚀 Features

- Workflow output ux updates (#1294)
- Add healthcheck endpoint (#1325)
- Add Okta authentication support and related documentation (#1328)
- Implement smart scrolling for message threads (#1330)
- Add authentication for local mode (#1340)

### 🐛 Bug Fixes

- Intermediate LLM calls during agent execution were not tracking token usage
- Intermediate LLM calls during agent execution were not tracking token usage (#1329)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 6 updates (#1336)
- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 5 updates (#1338)

### 📚 Documentation

- Remove authentication docs (#1326)

### ⚙️ Miscellaneous Tasks

- Bump crates typo version
- *(main)* Release (#1324)

## [0.3.13] - 2025-10-30

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 4 updates (#1314)
- *(deps)* Bump home from 0.5.11 to 0.5.12 in the prod-cargo-minor-dependencies group across 1 directory (#1308)
- *(deps)* Bump actions/upload-artifact from 4 to 5 (#1309)
- *(deps)* Bump actions/download-artifact from 5 to 6 (#1310)
- *(deps-dev)* Bump eslint-plugin-unicorn from 61.0.2 to 62.0.0 in the dev-npm-major-dependencies group (#1311)
- *(deps)* Bump the prod-npm-minor-dependencies group across 1 directory with 11 updates (#1322)

### ⚙️ Miscellaneous Tasks

- Release 0.3.13
- *(main)* Release (#1320)

## [0.3.12] - 2025-10-29

### 🚀 Features

- Refactor semantic docs and update topic doc (#1299)
- Motherduck support (#1304)
- Db / host overrides via api `connections` param (#1300)

### 🐛 Bug Fixes

- Typing lags with long threads (#1306)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-major-dependencies group with 5 updates (#1279)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1305)

## [0.3.11] - 2025-10-23

### 🚀 Features

- Support topic default filters (#1259)

### 🐛 Bug Fixes

- Handle session variable arrays via multiple requests (#1267)

### 💼 Other

- *(deps)* Bump the prod-cargo-minor-dependencies group across 1 directory with 13 updates (#1292)
- *(deps)* Bump the prod-npm-major-dependencies group with 3 updates (#1278)
- Update Node.js base image to version 24-slim in Dockerfile

### 🚜 Refactor

- Remove Docker build and publish jobs from public release workflow

### 📚 Documentation

- Add an enterprise license README (#1289)
- Update Copilot instructions and add Cline rules for Oxy development

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1297)

## [0.3.10] - 2025-10-22

### 🐛 Bug Fixes

- Correct matrix target reference in concurrency group for Docker publish job

### ⚙️ Miscellaneous Tasks

- Release 0.3.10
- *(main)* Release (#1295)

## [0.3.9] - 2025-10-22

### 🚀 Features

- Separate stage for preparing semantic layer and avoid docker in docker (#1284)

### 🐛 Bug Fixes

- Add missing EOF and conditional closure in deployment summary step
- Domo sync fail (#1293)

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group with 10 updates (#1276)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1291)

## [0.3.8] - 2025-10-22

### 🐛 Bug Fixes

- Force DECIMAL to float (#1290)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 10 updates (#1277)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1287)

## [0.3.7] - 2025-10-21

### 🚀 Features

- Support reading address and other connection info from env (#1269)
- Support now function to get current time (#1283)

### 🐛 Bug Fixes

- Cube dir isolation and clean up before rebuild (#1264)
- Agent stop on invalid topic, dimensions required in workflow task. (#1268)
- Update condition to exclude GitHub Actions from triggering move-code job
- Parsing for cube / semantic sql queries (#1270)
- Closes #1113 ide issues (#1256)
- Remove unused keys to prevent parse error (#1285)

### 💼 Other

- *(deps)* Bump actions/setup-node from 5 to 6 (#1275)

### 📚 Documentation

- Add domo documentation (#1260)

### ⚙️ Miscellaneous Tasks

- Format codes
- Update pnpm
- *(main)* Release (#1265)

## [0.3.6] - 2025-10-16

### 🚀 Features

- Clickhouse row filtering via api (#1212)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1258)

## [0.3.5] - 2025-10-15

### 🚀 Features

- Base view support (#1249)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1253)

## [0.3.4] - 2025-10-14

### 🚀 Features

- Semantic layer entities support composite keys (#1229)

### 🐛 Bug Fixes

- Semantics bug (#1237)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.38.0 to 1.38.1 (#1236)

### 📚 Documentation

- Semantic layer docs (#1230)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1231)

## [0.3.3] - 2025-10-12

### 🚀 Features

- Use cloud flag for oxy serve (#1224)

### 🐛 Bug Fixes

- Failed to parse some valid clickhouse queries (#1217)
- Sync Now should only show when versions different (#1220)
- Update TypeScript target and lib to ES2022

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group across 1 directory with 2 updates (#1223)

### ⚙️ Miscellaneous Tasks

- Format code
- Relax some deps
- *(main)* Release (#1219)

## [0.3.2] - 2025-10-08

### 🚀 Features

- Apply workspace concept (#1141)
- Semantic measure filters (#1149)
- Implement agentic workflow (#1151)
- Add sync apis for workflows (#1148)
- Omni integration (#1146)

### 💼 Other

- *(deps)* Bump crate-ci/typos from 1.36.2 to 1.36.3 (#1172)
- Reduce OS version for docker
- Need git to build deps
- *(deps)* Bump peter-evans/repository-dispatch from 3 to 4 (#1201)
- Add crates omni to build version
- *(deps)* Bump crate-ci/typos from 1.36.3 to 1.38.0 (#1203)

### 🎨 Styling

- Format

### ⚙️ Miscellaneous Tasks

- Add notes in dockercompose about app
- Upgrade rust version
- Remove unused packages
- Upgrade rust version
- Lock version fo crate ci typos
- Lock version fo crate ci typos
- Upgrade pnpm packages
- Upgrade packages
- Dont sync back pr from github actions
- Fix typo
- Add omni to linked version
- *(main)* Release (#1142)

## [0.3.1] - 2025-09-23

### 🚀 Features

- Support ask without streaming for v 0.3.x, fix apidoc page, fix apikey auth. drop custom auth (#1123)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 7 updates (#1126)
- *(deps)* Bump lewagon/wait-on-check-action from 1.4.0 to 1.4.1 (#1124)

### ⚙️ Miscellaneous Tasks

- Set fetch-depth to 0 for actions/checkout in CI jobs
- Format
- Upgrade deps
- *(main)* Release (#1137)

## [0.3.0] - 2025-09-19

### 🐛 Bug Fixes

- Ensure empty descriptions are not part of retrieval inclusions (#1117)

### ⚙️ Miscellaneous Tasks

- Release 0.3.0
- Cargo clippy
- *(main)* Release (#1118)

## [0.2.29] - 2025-09-18

### 🚀 Features

- Semantic engine api for workflow and routing agent (#1032)
- General yaml retrieval (#1108)

### 🐛 Bug Fixes

- Cube db type translation (#1109)

### 💼 Other

- *(deps)* Bump axios from 1.11.0 to 1.12.0 in the npm_and_yarn group across 1 directory (#1092)
- *(deps)* Bump actions/github-script from 7 to 8 (#1099)

### 📚 Documentation

- Add kubernetes documentation (#1094)
- Update installation method headings for clarity

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#1090)
- Update dependencies to latest versions across multiple crates
- Update dependencies in package.json
- Upgrade vite
- Release 0.2.29
- Upgrade version for oxy semantic
- Apply formatting
- *(main)* Release (#1111)

## [0.2.28] - 2025-09-10

### 🚀 Features

- Add auto reveal feature for ide sidebar (#1075)

### 🐛 Bug Fixes

- Oxy sync for clickhouse not working as expected (#1072)
- Web-app pnpm lock
- Default loop select & discard groups changes (#1071)
- Reintroduce port finding logic with attempts (#1086)
- Add git installation to Dockerfiles for dependency management

### 💼 Other

- *(deps)* Bump actions/setup-node from 4 to 5 (#1080)
- *(deps)* Bump actions/setup-python from 5 to 6 (#1079)
- Restore action setup pnpm
- *(deps-dev)* Bump vite from 6.3.5 to 6.3.6 in the npm_and_yarn group across 1 directory (#1088)

### ⚙️ Miscellaneous Tasks

- Upgrade packages versions
- Not create log file if not necessary
- Reorganize packages
- Leave pnpm caching for setup node
- *(main)* Release (#1069)

## [0.2.27] - 2025-09-03

### 🚀 Features

- Enable HTTPS and HTTP/2 support for local development (#1033)
- Add sentry to our product (#1042)
- Add VITE_SENTRY_DSN argument and environment variable to Dockerfile
- Implement responsive split pane for editor and preview, enhance layout adaptability (#1054)
- Implement usePersistedViewport hook for saving and loading workflow view state (#1058)

### 🐛 Bug Fixes

- Support Snowflake key, fix ollama (gpt-oss), and add additional debug logging (#1046)
- Ordering of items in ide sidebar (#1053)

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group with 2 updates (#1064)
- *(deps-dev)* Bump lint-staged from 16.1.5 to 16.1.6 in the dev-npm-minor-dependencies group (#1061)

### 🚜 Refactor

- Expose resolve_state_dir function and update key file path resolution (#1067)

### ⚙️ Miscellaneous Tasks

- Upgrade packages
- Update packages version for frontend
- Upgrade rust version
- Update arrow version  (#1050)
- Update tooltip message
- Update icons for workflow runs
- Upgrade some packages
- Specify rust 1.89 in dockerfile
- Update frontend packages
- *(main)* Release (#1044)

## [0.2.26] - 2025-08-27

### 🚀 Features

- Support global semantics within agent context (#1040)

### ⚙️ Miscellaneous Tasks

- Cargo clippy
- *(main)* Release (#1041)

## [0.2.25] - 2025-08-21

### ⚙️ Miscellaneous Tasks

- Release 0.2.5
- *(main)* Release (#1031)

## [0.2.24] - 2025-08-21

### 🚀 Features

- Add support for headers - useful for platforms like Helicone (#996)
- Refactor retrieval-related storage + adjust epsilon filtering (#1000)
- Improve topic replay/subscribe (#1026)

### 🐛 Bug Fixes

- Titles of chart and data table are inconsistent (#1005)
- Typo in missing anthropic api key message (#998)
- Add atomic transaction for new run
- Support type conversion so postgres can work (#1024)
- Remove redundant state tracking and add stream close handling (#1029)

### 💼 Other

- *(deps)* Bump lucide-react from 0.525.0 to 0.536.0 in the prod-npm-minor-dependencies group across 1 directory (#1007)
- *(deps)* Bump actions/download-artifact from 4 to 5 (#1015)
- *(deps)* Bump actions/checkout from 4 to 5 (#1014)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 6 updates (#1012)
- *(deps)* Bump the prod-npm-minor-dependencies group with 3 updates (#1017)
- *(deps)* Bump slab from 0.4.10 to 0.4.11 in the cargo group across 1 directory (#1019)

### ⚙️ Miscellaneous Tasks

- Upgrade deps
- Upgrade deps
- Remove conditions workflow
- *(main)* Release (#1009)

## [0.2.23] - 2025-08-07

### 🚀 Features

- Workflow replay (#984)

### 🐛 Bug Fixes

- App preview validation (#993)

### ⚙️ Miscellaneous Tasks

- Update dependencies
- Update TypeScript build info version to 5.9.2
- Run formatters
- Upgrade deps
- *(main)* Release (#995)

## [0.2.22] - 2025-07-30

### 🚀 Features

- Validate vibe coded app config (#989)

### 🐛 Bug Fixes

- Follow-up question immediately show in chat (#985)
- Multiple usability errors (#988)
- Auto reconnect/stream request when document hidden (#990)
- Add ide loading state (#991)

### ⚙️ Miscellaneous Tasks

- Upgrade deps
- Upgrade deps
- *(main)* Release (#987)

## [0.2.21] - 2025-07-24

### 🚀 Features

- Oxy clean (#979)

### 🐛 Bug Fixes

- Add guard clause for user in avatar (#977)

### 💼 Other

- *(deps)* Bump the npm_and_yarn group across 1 directory with 2 updates (#976)

### ⚙️ Miscellaneous Tasks

- Upgrade frontend deps
- *(main)* Release (#978)

## [0.2.20] - 2025-07-18

### 🚀 Features

- Create new settings page (#963)

### ⚙️ Miscellaneous Tasks

- Convert all step in step map to continuous form
- Release 0.2.20
- *(main)* Release (#973)

## [0.2.19] - 2025-07-17

### 🐛 Bug Fixes

- Runtime error message not returned (#971)

### ⚙️ Miscellaneous Tasks

- Release 0.2.19
- *(main)* Release (#972)

## [0.2.18] - 2025-07-16

### 🚀 Features

- Closes #951 observability panel (#952)
- Improve error handling (#959)
- Closes #955 show artifact panel in agent preview (#956)

### 🐛 Bug Fixes

- Improve error handling and logging in app data processing
- Remove redundant logging of web app URL during shutdown
- Enhance logging configuration for better control over HTTP and database logging levels
- Sidebar footer (#962)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 2 updates (#950)

### 📚 Documentation

- Add api docs for thread (#961)

### 🎨 Styling

- Improve UI/UX logs page (#957)

### ⚙️ Miscellaneous Tasks

- Format code
- Upgrade pnpm
- Add database information in sql files
- Upgrade deps
- Format codes
- *(main)* Release (#954)

## [0.2.17] - 2025-07-09

### 🚀 Features

- Enhance parameter resolution with type conversion for schema validation (#941)
- Git integration/readonly mode (#915)

### 🐛 Bug Fixes

- Add back altifacts (#948)

### ⚙️ Miscellaneous Tasks

- Re-structure api services web-app (#938)
- Update frontend dependencies
- Revert change from serde yaml to serde yml
- Upgrade dependencies
- *(main)* Release (#942)

## [0.2.16] - 2025-07-02

### 🚀 Features

- Implement semantic typing (#913)
- Add user role to db and entity (#922)

### 🐛 Bug Fixes

- Openapi doc not accessible (#939)

### 💼 Other

- *(deps)* Bump dotenv from 16.6.1 to 17.0.0 in the prod-npm-major-dependencies group (#934)
- *(deps)* Bump lewagon/wait-on-check-action from 1.3.4 to 1.4.0 (#930)
- *(deps)* Bump lucide-react from 0.522.0 to 0.525.0 in the prod-npm-minor-dependencies group (#929)

### 🚜 Refactor

- Closes #910 implement first message flow with persistent ai streaming and seamless page transitions (#925)

### ⚙️ Miscellaneous Tasks

- Update all deps
- Cargo update
- Add global semantics schema and improve error handling in workflow variable serialization (#928)
- Update dependencies and rust version
- Enhance dependency management in Vite config with structured chunking (#936)
- Update pnpm
- Update dependencies in Cargo.toml
- *(main)* Release (#926)

## [0.2.15] - 2025-06-26

### 🚀 Features

- Add sidebar item for database management (#914)

### 🐛 Bug Fixes

- Artifact panel scroll height (#924)

### 💼 Other

- *(deps-dev)* Bump typescript-eslint from 8.34.1 to 8.35.0 in the dev-npm-minor-dependencies group (#919)
- *(deps)* Bump the prod-npm-minor-dependencies group with 3 updates (#916)
- Enhance commit and workflow links in environment variable parsing
- Update package manager version to pnpm@10.12.3

### 📚 Documentation

- Add docs for routing agents (#923)

### ⚙️ Miscellaneous Tasks

- Unify ci permissions
- Upgrade deps
- Change in dist should not show after build
- Release 0.2.15
- *(main)* Release (#921)

## [0.2.14] - 2025-06-20

### 🚀 Features

- Closes #848 use data app primitives to power visualize tool  (#906)

### 🐛 Bug Fixes

- Clickhouse connector panick (#909)

### 💼 Other

- Fix build script for docker simple

### ⚙️ Miscellaneous Tasks

- Reorganize api key docs
- Upgrade deps
- Format code
- *(main)* Release (#912)

## [0.2.13] - 2025-06-17

### 🚀 Features

- Token counter (#885)

### 🐛 Bug Fixes

- Apply auth for register file duckDB (#903)

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group with 2 updates (#900)
- *(deps-dev)* Bump typescript-eslint from 8.34.0 to 8.34.1 in the dev-npm-minor-dependencies group (#899)
- Add simple Dockerfile and publish workflow for Docker images (#905)

### ⚙️ Miscellaneous Tasks

- Run cargo clippy fix
- *(main)* Release (#904)

## [0.2.12] - 2025-06-16

### 🚀 Features

- Closes #883 end user authentication (#884)
- Api keys support (#888)

### 🐛 Bug Fixes

- Get base url from origin header for google auth (#893)
- Logout (#894)
- Artifact view result table is big, it overrides the the sql view (#895)
- Add authorization token to fetchSSE headers (#896)

### 📚 Documentation

- Add reference architecture and improve authentication documentation (#897)

### ⚙️ Miscellaneous Tasks

- Upgrade dependencies
- Update pnpm
- *(main)* Release (#892)

## [0.2.11] - 2025-06-13

### 🚀 Features

- Sync column description and table description (#872)
- Impl error handing and migrate json streaming to sse (#876)
- Implement Cognito authentication and enhance logout functionality (#882)
- Improve cognito jwt parsing process (#889)
- Implement artifact events handler (#877)

### 🐛 Bug Fixes

- Improve logging in cloudrun (#875)
- Update log format detection for Cloud Run compatibility
- Disable ANSI output in logging for better compatibility
- Disable timestamp in cloudrun logging for cleaner output
- Jwt needs padding handler for alb
- Revert to using base64 for amazon cognito

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group with 3 updates (#868)
- Add deepwiki badge for auto-reindexing (#866)
- *(deps)* Bump the prod-npm-minor-dependencies group across 1 directory with 5 updates (#886)

### 🚜 Refactor

- Simplify user info fetching and clear everything when log out (#874)
- Use json web token to decode jwt
- Implement auth-mode-specific logout handling and improve lo… (#891)

### ⚙️ Miscellaneous Tasks

- Update oxy dependencies
- Remove short SHA from nightly build manifest creation
- Remove concurrency settings from opensource workflow
- Upgrade packages
- Format and fix ci
- Update config schema
- *(main)* Release (#873)

## [0.2.10] - 2025-06-02

### 🚀 Features

- Make ai artifact unverified (#860)
- Add bulk delete api and deletion ui (#854)

### 🐛 Bug Fixes

- Support file name with space (#838)
- Markdown link color (#864)
- Unnecessary messages fetches (#865)

### 💼 Other

- *(deps)* Bump the npm_and_yarn group across 1 directory with 2 updates (#862)

### 📚 Documentation

- Add back development docs

### ⚙️ Miscellaneous Tasks

- Add concurrency settings to opensource workflow
- Update dependencies
- Cargo updates
- Expose agent api (#863)
- Update deps
- *(main)* Release (#861)

## [0.2.9] - 2025-05-29

### 🐛 Bug Fixes

- White content inside button
- Remove all Button-related text-white and apply global style
- Threads list layout (#855)

### 🚜 Refactor

- Simply pagination logic

### ⚙️ Miscellaneous Tasks

- Release 0.2.9
- *(main)* Release (#853)

## [0.2.8] - 2025-05-29

### 🐛 Bug Fixes

- Hotfix case when there are only 2 pages

### ⚙️ Miscellaneous Tasks

- Release 0.2.8
- *(main)* Release (#852)

## [0.2.7] - 2025-05-29

### 🚀 Features

- Memory and n-threads chat (#828)
- Adds paginated thread API and virtualized thread list (#839)
- Add database sync/build API and oxy sync oxy build to the frontend  (#850)
- Improve app usability (#843)

### ⚙️ Miscellaneous Tasks

- N threads ui enhancement (#841)
- Cargo fix and fmt
- Update packages
- *(main)* Release (#840)

## [0.2.6] - 2025-05-27

### 🚀 Features

- User isolation & iap authentication (#827)

### 🐛 Bug Fixes

- Custom migration on sqlite dialect (#835)

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group with 3 updates (#829)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 2 updates (#830)
- *(deps)* Bump the npm_and_yarn group across 1 directory with 2 updates (#833)

### ⚙️ Miscellaneous Tasks

- Release 0.2.6
- Upgrade deps uuid
- Update pnpm lock
- Resize ava (#836)
- *(main)* Release (#826)

## [0.2.5] - 2025-05-23

### 🐛 Bug Fixes

- Improve delete handling in FileNode and DirNode components (#824)

### ⚙️ Miscellaneous Tasks

- Avoid using native tls
- Release 0.2.5
- *(main)* Release (#825)

## [0.2.4] - 2025-05-23

### 🚀 Features

- Add styling for thread not found
- Closes #803 create file, edit file name, remove file in ide (#811)

### 🐛 Bug Fixes

- Typo in artifact tooltip (#821)
- Reduce amount of logs for http logging  (#823)

### 📚 Documentation

- Improve docs
- Add ec2 instance setup docs
- Update Docker image version to 0.2.3 in deployment guide

### ⚙️ Miscellaneous Tasks

- Remove view artifact functionality
- Opensource on click
- Release 0.2.4
- *(main)* Release (#820)

## [0.2.3] - 2025-05-22

### 🚀 Features

- Hide data app (#800)
- Unify all into thread (#802)
- Support download query result csv (#810)
- Save query to workflow (#809)
- Add skipped files and overwritten files tracking & add docs and test for oxy sync (#808)
- Implement synthesize step for routing agent (#814)
- Improve page loading state (#816)

### 🐛 Bug Fixes

- Sql type not return in thread (#817)
- Page layout with thread references (#818)

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group with 2 updates (#806)
- Add docker nightly build
- Build multi arch docker

### 📚 Documentation

- Add docs for deployment
- Add docker deployment
- Reorganize mermaid chart and deployment steps
- Reorganize docs folder
- Avoid using latest tag for docker
- Remove duplicate first-level headings
- Improve docs
- Keep completing docs
- Updates docs
- Update chart
- Update documentation
- Reorganize deployment docs
- Reorganize docs
- Reorganize docs
- Remove commented redirection in error handling for Docker and reverse proxY

### ⚙️ Miscellaneous Tasks

- Update web deps
- Update frontend deps
- Cargo fix
- Set docker tag as version
- Update Radix UI and other dependencies to latest versions
- Add arch key to concurrency to avoid jobs cancelling
- Fix docker build on matrix
- Update packages
- *(main)* Release (#801)

## [0.2.2] - 2025-05-19

### 🚀 Features

- Table render to follow json value format (#785)

### 🐛 Bug Fixes

- Oxy test relative path (#783)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#786)

## [0.2.1] - 2025-05-16

### 🚀 Features

- Unified structured logging for oxy (#776)
- Improve workflow diagram (#759)

### 🐛 Bug Fixes

- Add scrollbar to query box (#775)
- Can not render vega chart (#780)

### 💼 Other

- *(deps-dev)* Bump lint-staged from 15.5.2 to 16.0.0 in the dev-npm-major-dependencies group across 1 directory (#769)

### 🚜 Refactor

- Unify stdout and file logging through appender (#778)

### 📚 Documentation

- Add debugging docs and reorganize docs folder

### ⚙️ Miscellaneous Tasks

- Update cargo deps
- Cargo fix
- Add vite build to CI
- Bump version of pnpm
- *(main)* Release (#777)

## [0.2.0] - 2025-05-13

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group across 1 directory with 10 updates (#768)
- *(deps)* Bump fpicalausa/remove-stale-branches from 2.3.0 to 2.4.0 (#760)

### 📚 Documentation

- Add link to internal wiki for additional documentation

### ⚙️ Miscellaneous Tasks

- Add workflow_dispatch input for branch synchronization
- Comment out push event triggers in workflow
- Update condition for move-code job to include workflow_dispatch event
- Restore push event triggers in workflow and update condition for move-code job
- Release 0.2.0
- *(main)* Release (#770)

## [0.1.23] - 2025-05-08

### 🐛 Bug Fixes

- Re-add vega

### ⚙️ Miscellaneous Tasks

- Release 0.1.23
- *(main)* Release (#758)

## [0.1.22] - 2025-05-08

### 💼 Other

- *(deps)* Bump the npm_and_yarn group across 1 directory with 2 updates (#749)
- Add error boundary to data app page to prevent crashes (#754)
- *(deps)* Bump the prod-npm-minor-dependencies group across 1 directory with 3 updates (#753)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 6 updates (#752)
- Disable builder toggle when builder agent is not available (#755)

### 📚 Documentation

- Add docs for data apps
- Add link to deep wiki

### ⚙️ Miscellaneous Tasks

- Remove some unused packages
- *(main)* Release (#757)

## [0.1.21] - 2025-05-07

### 🚀 Features

- Add login, logout and user information (#663)
- Estimate cost before running query for biquery (#669)
- Persist chart in chart dir and refactor statedir (#668)
- Support execute query generated from Omni semantic model (#664)
- Add data app support (#678)
- Closes #661 SQL and YAML editor (#662)
- Cache assests (#707)
- Support data app live editor (#711)
- Data app editor enhancement (#712)
- Extend oxy make to support bigquery / support multiple datasets in bigquery (#683)
- Closes #690 dark theme for sidebar, main app experience (#713)
- New color for our app (#727)
- Closes #695 output panel for workflow (#740)
- Psudo data app vibe coding (#729)
- Add is_partition_key flag to semantic dimensions (#738)
- Improve UI/UX tasks, app sidebar (#745)
- Add duckdb schema exploration (#719)
- Add example data app to sleep data example (#748)

### 🐛 Bug Fixes

- Improve dry run query parsing and update logging level to debug
- Docs missing group name
- Update job names and rustfmt-check version in CI workflow
- Update CI job to use aws runner for testing
- Update CI workflow to use custom profile for cargo commands
- Chart fails to render (#706)
- Duckdb init multiple times (#708)
- Bar chart loading (#709)
- Ensure scrollbar always appear on the right hand side (#710)
- Chart readability (#728)
- Visual fixes (#741)
- UI bugs always stuck at Fail to load app (#747)

### 💼 Other

- *(deps)* Bump react-router from 7.5.1 to 7.5.2 in the npm_and_yarn group (#673)
- *(deps)* Bump the npm_and_yarn group across 1 directory with 2 updates (#670)
- *(deps-dev)* Bump eslint-plugin-unicorn from 58.0.0 to 59.0.0 in the dev-npm-major-dependencies group (#686)
- *(deps)* Bump @radix-ui/react-tabs from 1.1.8 to 1.1.9 in the prod-npm-minor-dependencies group across 1 directory (#698)
- *(deps-dev)* Bump vite from 6.3.3 to 6.3.4 in the npm_and_yarn group (#716)
- *(deps-dev)* Bump vite from 6.3.3 to 6.3.4 in /web-app in the npm_and_yarn group across 1 directory (#715)
- Replace default linker with mold (#714)
- *(deps)* Bump the prod-npm-minor-dependencies group with 18 updates (#732)
- *(deps)* Bump the prod-npm-major-dependencies group with 2 updates (#734)
- *(deps-dev)* Bump @types/node from 22.15.11 to 22.15.12 in the dev-npm-minor-dependencies group across 1 directory (#737)

### 🚜 Refactor

- Consolidate cargo check and test jobs into a single workflow step

### 🧪 Testing

- Fix some test result

### ⚙️ Miscellaneous Tasks

- Use default cargo opt level
- Move the /not_signed_in route into mainlayout
- Set base for app
- Change from database url to oxy database url
- Update some typos
- Make tests pass
- Update json schema
- Remove unused function since we dont have sensitive files
- Update CI workflow  (#682)
- Format codes
- Trigger ci build
- Update CI workflow to use cargo run for schema checks and adjust profile settings
- Update cargo check and test job to use matrix for instance sizing
- Try new version of rust cache
- Trigger test run
- Remove yarn lock
- Try optimization with buildspecs override
- Try optimization using buildspec yml
- Try optimization using buildspec yml
- Update buildspec.yml and Cargo.toml for improved workspace setup
- Migrate from aws codebuild due to issue with caching
- Disable comment on pr for workflow telemetry
- Update axum dependency to version 0.8.4
- Update examples
- Update dependencies
- Remove some errors in chart
- Update cargo lock file
- Add explicit type annotations for entry and model to resolve type inference errors
- Run cargo fix and cargo fmt
- Run cargo fix
- Cache data file (#742)
- Use duckdb wasm from cdn for web (#743)
- Hide builder agent from agent list (#744)
- *(main)* Release (#667)

## [0.1.20] - 2025-04-22

### 💼 Other

- *(deps)* Bump docker/login-action from 2 to 3 (#644)
- *(deps)* Bump the prod-npm-minor-dependencies group with 16 updates (#645)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 4 updates (#646)
- *(deps)* Bump the prod-npm-minor-dependencies group with 2 updates (#656)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 4 updates (#660)

### ⚙️ Miscellaneous Tasks

- Update json schema
- Add launch json file for easier debugging
- Ignore changes to launch.json
- Remove optimization level customization
- Reset level of optimization for profiles
- Update dependencies
- Update dependabot config
- Refine cargo directories in dependabot config
- Simplify cargo directories in dependabot config
- Revert examples to 4o to optimize cost when testing
- Upgrade pnpm to 10.9.0
- Reduce opt level to 0 for dev
- Change rustls backend to aws lc
- Release 0.1.20
- *(main)* Release (#643)

## [0.1.19] - 2025-04-18

### 🐛 Bug Fixes

- Fallback to single sql type when cache key found (#641)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#640)

## [0.1.18] - 2025-04-18

### 🐛 Bug Fixes

- Improve visualize tool output (#638)

### ⚙️ Miscellaneous Tasks

- Allow nightly build to share cache with other releases
- *(main)* Release (#639)

## [0.1.17] - 2025-04-17

### 🐛 Bug Fixes

- Update MCP server log message to include localhost URL

### ⚙️ Miscellaneous Tasks

- Remove warning because it will run again & again
- *(main)* Release (#637)

## [0.1.16] - 2025-04-17

### 🐛 Bug Fixes

- Force using ring as default for rustls

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#636)

## [0.1.15] - 2025-04-17

### 🚀 Features

- Dockerize oxy and enable postgres as a data backend (#632)

### 🐛 Bug Fixes

- Always set current dir to project path for mcp stdio, force web server to be started in project path (#634)

### 💼 Other

- *(deps-dev)* Bump vite from 6.3.0 to 6.3.1 in /web-app in the npm_and_yarn group across 1 directory (#633)

### 📚 Documentation

- Fix BigQuery requires the dataset field

### ⚙️ Miscellaneous Tasks

- Remove last release sha
- Add save-if condition for caching in workflows
- Remove cache-on-failure option from cargo cache steps
- Upgrade deps
- *(main)* Release (#635)

## [0.1.14] - 2025-04-17

### 🚀 Features

- #573 Visualization meta issue (#590)
- Handle bigquery source directly and increase timeout (#622)
- Allow agents to execute workflows (#618)
- Closes #630 Improve agent loading state (#631)
- Implement stream retry (#629)

### 🐛 Bug Fixes

- Mcp sse not working
- Using api_key in azure (#627)
- Add structured response description (#628)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 6 updates (#612)
- *(deps)* Bump the prod-npm-minor-dependencies group with 20 updates (#611)
- *(deps-dev)* Bump vite from 6.2.6 to 6.3.0 in /web-app in the npm_and_yarn group across 1 directory (#624)

### 📚 Documentation

- Update clickhouse.mdx
- Update clickhouse.mdx
- Update postgres.mdx
- Update redshift.mdx
- Update snowflake.mdx
- Update docs (#626)

### ⚙️ Miscellaneous Tasks

- Adjust workflow to create cache differently
- Remove `builders` flag, remove intermediate funcs and legacy code (#614)
- Update TypeScript build info and pnpm
- Update dependencies in Cargo.toml
- Update json schemas
- *(main)* Release (#619)
- *(main)* Release (#620)
- Update all 4o to 4.1 (#625)
- Release 0.1.14
- Fix release-please
- Fix release-please
- *(main)* Release (#623)

## [0.1.12] - 2025-04-14

### 🚀 Features

- Support multiple embed documents for a sql file (#605)

### ⚙️ Miscellaneous Tasks

- *(main)* Release (#607)

## [0.1.11] - 2025-04-11

### 🚀 Features

- Add Anthropic/Claude model support (#582)
- Add more information to build
- Arm linux build
- Improve embedding (#593)
- Add openapi docs for customer facing API (#589)
- Navigate to subworkflow (#601)

### 🐛 Bug Fixes

- Update installation command for nightly release to include -L flag

### 💼 Other

- *(deps)* Bump crossbeam-channel from 0.5.14 to 0.5.15 in the cargo group across 1 directory (#591)
- *(deps-dev)* Bump vite from 6.2.5 to 6.2.6 in the npm_and_yarn group (#597)
- *(deps-dev)* Bump vite from 6.2.5 to 6.2.6 in /web-app in the npm_and_yarn group across 1 directory (#595)
- *(deps)* Bump crossbeam-channel from 0.5.14 to 0.5.15 in the cargo group across 1 directory (#592)

### ⚙️ Miscellaneous Tasks

- Cargo clippy
- Update schemas
- Re-architect executable (#499)
- Typo in feature
- Allow nightly release to be published when run manually
- Lock rust version for nightly build
- Switch internal release job to warp build
- *(main)* Release (#588)

## [0.1.10] - 2025-04-08

### 🚀 Features

- Nightly build for oxy (#568)
- Enhance nightly build script with unzip validation and error handling
- Update nightly build trigger conditions for scheduled and manual workflows
- Render condition task in workflow diagram (#578)
- #524 Build testing UI (#556)

### 🐛 Bug Fixes

- Update markdown links in workflow announcements for better readability
- Oxy panic and exit silently, load .env from project path when running mcp (#584)
- Ts typing (#585)

### 💼 Other

- *(deps)* Bump openssl from 0.10.71 to 0.10.72 in the cargo group across 1 directory (#569)
- *(deps)* Bump openssl from 0.10.71 to 0.10.72 in the cargo group across 1 directory (#570)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 9 updates (#575)
- *(deps)* Bump the prod-npm-minor-dependencies group with 8 updates (#576)
- *(deps)* Bump actions/create-github-app-token from 1 to 2 (#574)
- Unhide revert section in release-please configuration
- *(deps)* Bump tokio from 1.44.1 to 1.44.2 in the cargo group across 1 directory (#580)
- *(deps-dev)* Bump typescript-eslint from 8.29.0 to 8.29.1 in the dev-npm-minor-dependencies group (#579)

### 📚 Documentation

- Update internal and public docs

### ⚙️ Miscellaneous Tasks

- Fix markdown link when sending release announcement to slack
- Update release nightly workflow
- Update nightly build
- Fix condition for ubuntu
- Support installing oxy with sudo
- *(main)* Release (#583)
- *(main)* Release (#587)

## [0.1.9] - 2025-04-04

### 🚀 Features

- Refactor & stabilize mcp server (#560)
- Add workflow diagram control (#567)

### 🐛 Bug Fixes

- Source overflow when query is too long (#557)
- Agent datetime awareness (#558)
- Context value serialization fail (#566)

### 💼 Other

- *(deps-dev)* Bump vite from 6.2.4 to 6.2.5 in the npm_and_yarn group (#565)
- *(deps-dev)* Bump vite from 6.2.4 to 6.2.5 in /web-app in the npm_and_yarn group across 1 directory (#561)

### 📚 Documentation

- Mcp server (#563)

### ⚙️ Miscellaneous Tasks

- Update json schemas
- *(main)* Release (#555)
- Improve slack message
- Adjust fruit sales report workflow description
- Add descriptions to all example workflow
- *(main)* Release (#559)

## [0.1.8] - 2025-04-02

### 🚀 Features

- #518 implement if else logic for our yaml (#535)
- #550 support Jinja filters and data transformation for referenced variable (#554)
- *(beta)* Mcp server and improvements to web server (#553)

### 🐛 Bug Fixes

- Sort agent alphabetically and dont allow message to be sent if form is empty (#552)

### 💼 Other

- Add GitHub App token creation and repository dispatch for oxy_release event
- *(deps)* Bump tailwind-merge from 3.0.2 to 3.1.0 in the prod-npm-minor-dependencies group (#549)
- *(deps-dev)* Bump typescript-eslint from 8.28.0 to 8.29.0 in the dev-npm-minor-dependencies group (#548)

### ⚙️ Miscellaneous Tasks

- Add a public release announcement
- *(main)* Release (#551)

## [0.1.7] - 2025-03-31

### 🚀 Features

- Oxy make (#538)
- Add port support for serve (#540)

### 💼 Other

- *(deps)* Bump fpicalausa/remove-stale-branches from 2.2.0 to 2.3.0 (#542)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 2 updates (#544)
- *(deps-dev)* Bump vite from 6.2.3 to 6.2.4 in /web-app in the npm_and_yarn group across 1 directory (#545)
- *(deps)* Bump the prod-npm-minor-dependencies group with 13 updates (#543)

### 🚜 Refactor

- Remove agent configuration and update database handling in CLI

### ⚙️ Miscellaneous Tasks

- Run CI with example empty gemini api key
- Remove comments from json
- *(main)* Release (#541)

## [0.1.6] - 2025-03-28

### 🚀 Features

- Support running api and web on same port (#536)

### ⚙️ Miscellaneous Tasks

- Refine public release changelog
- *(main)* Release (#537)

## [0.1.5] - 2025-03-28

### 🚀 Features

- Enhance query ref ui (#509)
- Add clickhouse & mysql support (#508)
- #510 delete thread (#520)
- Gemini support (#521)
- Self-update to check for latest update (closes #132) (#525)
- Delete all threads (#526)
- Support snowflake and read env from .env (#527)

### 🐛 Bug Fixes

- *(#410)* Handling of multibytes char inside pluralizer (#522)
- Figure out another port when one is busy (#528)
- Get schema not working for snowflake (#534)

### 💼 Other

- *(deps)* Bump the prod-npm-minor-dependencies group with 7 updates (#513)
- *(deps-dev)* Bump eslint-plugin-unicorn from 57.0.0 to 58.0.0 in the dev-npm-major-dependencies group (#515)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 9 updates (#517)
- Improve build time for lancedb by static linking
- Add note for arrow version
- *(deps)* Bump axios from 1.8.3 to 1.8.4 in /web-app in the npm_and_yarn group across 1 directory (#516)
- *(deps-dev)* Bump typescript-eslint from 8.27.0 to 8.28.0 in the dev-npm-minor-dependencies group (#519)

### 📚 Documentation

- Add instruction on how to use docker compose
- Add docs for cache, fix #512 (#523)

### ⚙️ Miscellaneous Tasks

- Change from onyx to oxy in release event
- Remove unused copybara_options from GitHub Actions workflow
- Add web-app to release please
- Enhance release workflows with unified changelog handling
- Update pnpm
- Update lancedb
- Update support information
- Remove default agent and add default db (#529)
- Change gemini vendor to google (#532)
- Remove clickhouse password because clickhouse does not have one
- *(main)* Release (#511)
- Build on lower ubuntu to have wider glibc compat

## [0.1.4] - 2025-03-21

### 🚀 Features

- Add sql query reference to agent answer (#502)
- Improve workflow output rendering (#504)

### 🐛 Bug Fixes

- Add missing header text (#500)
- Workflow gets highlighted when in thread page (#501)
- Broken link (#506)
- Update pull request permissions in CI workflow

### 📚 Documentation

- Add v0 getting started guide (#503)

### ⚙️ Miscellaneous Tasks

- Update permission for repository dispatch
- *(main)* Release (#507)

## [0.1.3] - 2025-03-18

### 🐛 Bug Fixes

- Load agent with answer (#492)
- Agent sql execution is not logged (#494)

### 📚 Documentation

- Add documentation for different databases (#491)
- Add serve, and update installation instructions (#496)
- Update link reference

### ⚙️ Miscellaneous Tasks

- Remove some space so tests can run successfully
- Enhance workflows
- Update concurrency group name in public release workflow
- Update permissions for pull-requests in clean-up-branches workflow
- Update video on readme
- Update concurrency group names in CI workflow
- Improve move code workflow
- Add workflow to trigger Homebrew update on new releases (#493)
- Remove tauri app
- Restore check for opensource
- Update Homebrew workflow to trigger on released and published events
- Collect workflow telemetry
- Remove reference to sample repo
- *(main)* Release (#495)

## [0.1.2] - 2025-03-18

### 🚀 Features

- Edit workflow (#351)
- #440 setup shadcn, implement layout & siderbar (#441)
- Closes #392 be able to run things multiple times and use consistent re… (#406)
- Add postgres connection  (#439)
- Support redshift (#445)
- Run and show workflow output + remove panda css and use shadcn in workflow diagram (#452)
- #442 implement chat & agent components (#443)
- Render sub-workflow variables before passing in (#455)
- Remove pandacss and unuse components (#456)
- Make workflow ui consistent with agent (#472)

### 🐛 Bug Fixes

- Update repository links to point to the main onyx repository
- Pass variable in as context into test (#453)
- Run workflow api (#458)
- Missing quote (#480)
- Broken formatting (#481)

### 💼 Other

- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 7 updates (#435)
- *(deps)* Bump the prod-npm-minor-dependencies group across 1 directory with 8 updates (#438)
- *(deps)* Bump remark-directive from 3.0.1 to 4.0.0 in the prod-npm-major-dependencies group (#436)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 8 updates (#448)
- *(deps)* Bump axios from 1.8.1 to 1.8.2 in the npm_and_yarn group (#449)
- *(deps)* Bump axios from 1.7.9 to 1.8.2 in /web-app in the npm_and_yarn group across 1 directory (#450)
- *(deps)* Bump the prod-npm-minor-dependencies group across 1 directory with 8 updates (#451)
- *(deps)* Bump the prod-npm-minor-dependencies group with 9 updates (#470)
- *(deps-dev)* Bump vite from 6.2.1 to 6.2.2 in the dev-npm-minor-dependencies group (#471)
- *(deps)* Bump axios from 1.8.2 to 1.8.3 in /web-app in the npm_and_yarn group across 1 directory (#468)
- *(deps-dev)* Bump @types/react from 19.0.10 to 19.0.11 in the dev-npm-minor-dependencies group (#484)

### 🚜 Refactor

- Remove tauri and use native modules (#446)

### 📚 Documentation

- Add alternative installation method
- Add internal docs for release process and instruction  (#437)
- Workflow variables, workflow task types, variable override (#460)
- Fix changes to oxy; remove outdated docs (#479)
- Update installation command URL in README
- Add badges and license section to README
- Update logo and remove outdated contribution info (#490)

### ⚙️ Miscellaneous Tasks

- Update installation script to work with latest version
- Generate release notes and update changelog in public release workflow
- Update public release workflow to auto trigger on version tags
- Refactor config to support multiple backends (#409)
- Update pnpm install command to use --prefer-frozen-lockfile
- Fix check not run when pr ready
- Split up examples and sample project (#433)
- Update docs and failing test
- Add 'dataset' property to JSON schemas and update required fields
- Remove desktop app build pipeline (rust backend) (#444)
- Fix test fail with steps -> tasks
- Enable cache on failure for cargo cache preparation
- Update command to check JSON schemas in CI workflow
- Update wording in consistency test output (#459)
- Fix pnpm lock conflict remaining (#463)
- Auto format markdown
- Auto format markdown and mdx
- Auto format markdown and mdx
- Allow the use of github markdown format
- Workflow api refactoring (#465)
- Open source the web ui
- Upgrade dependencies
- Upgrade deps
- Consensus runs -> consistency runs (#474)
- Update deps
- Rename to oxy
- Continue moving to oxy
- Update references from Onyx to Oxy in documentation and UI
- Update oxy logo (#478)
- Apply oxy tech domain
- Remove remark for markdown
- Remove remark for markdown
- Update docs logo (#482)
- Update Rust edition to 2024 and add rust version
- Add rust-version to workspace configuration in Cargo.toml files
- Add concurrency settings to CI jobs in workflow configuration
- *(main)* Release (#432)

## [0.1.1] - 2025-03-01

### 🚀 Features

- Feat: add fully functional demo with workflow, testing, agent  (#430)

### ⚙️ Miscellaneous Tasks

- Release 0.1.1
- *(main)* Release (#431)

## [0.1.0] - 2025-02-28

### 🚀 Features

- Include db schemas into instruction
- Integrate tools
- Improve aesthetic
- Implement vector search
- Reorganize config
- Scaffolding app
- Support ollama
- Rearrange llm configuration and update defaults config
- Add workflow execution
- Implement generic tool types
- Implement onyx validate
- Move system instructions to upper level
- Change scope -> data and support multiple paths
- Add basic retry support
- Show dir project
- Sort dir
- Install script
- Add list agents api
- Stream answer
- Support execute_sql step
- Support path expansion for project_path of defaults (#57)
- Support conversation with agents
- Add agent updated_at, fix conversation not found
- Agent updated at
- Dynamic based on the local time
- Remove makefile and use turbo
- Truncate to max 100 rows by default
- Add legacy color support (close #77) (#82)
- Support sequential loop and formatter
- Support nested loop
- Turn sequential output into vec
- Improve j2 context to provide better templating
- Use vendored native-tls so we dont mess with local ssl on different linux systems (#97)
- Support building windows binary
- Anonymize data
- Implement pluralize and case_insensitive
- Print deanonymized output
- Make it so tables are responsive in terminal #95
- Add table output type
- Add json schema to loop
- Clean up python projects
- Narrow down mac os x deployment MACOSX_DEPLOYMENT_TARGET
- Support mapping anonymization (#153)
- Prep for semantic release automatically (#158)
- Add changelog
- Remove action rust lang because it makes caching harder (#160)
- Retry  #158 automatic release  by combining release-plz and release-please (#161)
- *(ENG-1167)* Separate out queries from tool context rename as context (#171)
- *(ENG-1171)* Allow for default warehouse argument in configyml (#174)
- Support taking in onyx version for instllation script
- Add release please boostrap
- Eng-1170 Support semantic models
- Add api url for openai model to support azure openai (#184)
- Hybrid search (#192)
- Improve logging and error handling (#209)
- Refactor context (#211)
- Implement execution progress (#229)
- Add integration tests (#219)
- *(eng-1173)* Support export argument on workflow steps (#198)
- Eng-1175 Create desktop app with Tauri (#233)
- Rename app to onyx-desktop and update dependencies in Cargo files
- Add installation script for Onyx with support for Linux and macOS (#284)
- Apply copybara to publish an opensource version of onyx (#293)
- Enhance GitHub Actions workflow to retrieve GitHub App User ID and update checkout action
- Create project selection and view project file tree (#283)
- Support caching for generated queries inside workflows (#290)
- Include some build args to ensure onyxpy build succeed with cargo build
- Update to new filesystem format, and use new positioning (#323)
- Add enabled key under cache key (#322)
- Re-organize everything to prep for opensource (#296)
- Build file tree extended functionalities (#311)
- Render workflow (#335)
- Add download component to our docs (#345)
- #305 Chat UI for Agent (#346)
- #382 the execute_sql step within workflows works should apply Jinja context from the workflow (#383)
- #308 Render Agent yaml config to desktop app UI (#372)
- Subworkflow and workflow variables new (#407)

### 🐛 Bug Fixes

- Remove deprecated model
- Typo in default config file
- Limit content passed to openai
- Resolve conflic
- Bugs
- Merge main
- Merge main
- Expected reference to Pathbuf, found Pathbuf error; also some auto-linting
- Dialect isn't valid (should be type)
- Add pnpm install to ensure deps are installed
- Add version for rust toolchain
- Install libssl dev on some platforms
- Build commands
- Dependency installation
- Openssl for linux
- Error: variant `Text` is never constructed
- Missing typo
- Build
- Merge main
- Merge main
- Move git config upward
- Remove path when checking out
- Credentials for github action
- Script should support arm mac
- Naming
- Format
- Conflict
- Conflict
- Format
- Coversation not found
- Default env
- Default database
- Web-app dist in binary
- Web-app dist in binary
- Web-app dist in binary (#64)
- Web-app dist in binary
- Remove unuse code
- Add fallback to index.html web-app
- Format
- Support all type
- Format
- Use comfy_table
- Use comfy_table
- Use comfy_table
- Fmt and remove unuse code
- Resolve conflic
- Correct filename casing for banner.png
- Revert back to ubuntu latest
- Remove search files feature
- Resolve code review
- Symlink should not be replaced by folder
- Onyx serve json->markdown format
- Remove exmaple config
- Merge main
- Print the result table even batches null
- Set-output command is deprecated and will be disabled soon
- Add format and add remote url to config.json
- Public release file patterns
- Merge main
- Clean code
- Typo, unuse code
- Only show footer when total_column > displayed_column
- Update all config file
- Remove unuse code
- Bug load agent name
- Fmt
- Workflow for release
- Merge main
- Merge main
- Add extension module so cargo build succeeds
- Merge main
- Refactor code problems with cargo
- Build error after upgrading react and types/react
- Serve with agent relative path (#150)
- Remove extra idx from keyword replacement (#157)
- Installation script missing arm64
- Bugs get ctx
- Bugs get ctx
- Clean code
- Clean code
- Clean code
- Merge main
- Conflict main
- Conflict main
- Code review
- Key_path should not be a required argument when warehouse.type = duckdb (#193)
- Update to onyx run syntax (#212)
- Validate result not showed on error (#213)
- Add JSON schema validation step to CI workflows
- Export panic when execute_sql failed (#246)
- Correct syntax for moving tauri assets in release workflow
- Update tauri asset movement in release workflow for correct handling
- Prefix artifact names with 'tauri-' and 'cli-' in release workflow
- Update SSH key reference in GitHub Actions workflow for Copybara
- Correct committer format in GitHub Actions workflow for opensource publishing
- Update lint-staged command to handle scheduled events differently
- Enable caching for all Rust crates in CI workflows
- Workaround for release please issue
- Workaround for release please issue
- Workaround for release please issue
- Workaround for release please issue
- Workaround for release please issue
- Workaround for release please issue
- Update Windows DIST path to correct directory
- Handle onyx init when no config is found (#340)
- Refactor cache into executor (#337)
- Remove unnecessary props and clean up default display in system config
- Lock duckdb version (#350)
- Ensure all packages are released together
- Lint errors (#353)
- Update push_move condition in GitHub Actions workflow
- Improve push_exclude condition in GitHub Actions workflow
- Run lint-staged in quiet mode during pre-commit hook
- Update push_exclude and push_move conditions in GitHub Actions workflow
- Update pr_exclude conditions in GitHub Actions workflow
- Update pr_exclude conditions and add copybara options in GitHub Actions workflow
- Remove destination branch and update push_exclude conditions in GitHub Actions workflow
- Remove copybara options from GitHub Actions workflow
- Update pr_exclude conditions in GitHub Actions workflow to include additional paths
- Update copybara action to use a different version in GitHub Actions workflow
- Remove release notes formatting steps from GitHub Actions workflow
- Update conditions for changelog and schema generation in workflows
- Update pr_move conditions in GitHub Actions workflow for better path handling
- Update pr_exclude conditions in GitHub Actions workflow and add opensource folder to .gitignore
- Correct pr_exclude pattern and add pr_move conditions in GitHub Actions workflow
- Update pr_exclude pattern to include onyx-desktop opensource folder in GitHub Actions workflow
- Refine pr_exclude pattern in GitHub Actions workflow for onyx-desktop
- Update pr_exclude pattern for onyx-desktop in GitHub Actions workflow
- Update push_exclude and pr_exclude patterns in GitHub Actions workflow for onyx-desktop
- Emojis with variant selector break tabled (#370)
- #373 chat with agent (#374)
- #376 fix issue where the released desktop version cannot connect to the database (#377)
- Ensure all packages are released together
- Classname is removed for markdown plugin
- Add sample project file

### 💼 Other

- Move web app dist to the right folder
- Enable cargo check to run with the right tools
- Move web-app/dist directory during CI workflow
- Move web-app/dist directory during CI workflow
- Add support for cross-compilation in release workflow
- Ensure release is always tagged
- Use app token for release workflow
- Update script
- Switch to nightly build instead
- Remove mv folder
- Update onyx version to 0.1.4
- Remove pnpm cache setup from release workflow
- Typo with cargo zig build
- Try building with older ubuntu version
- Enable windows build (#117)
- *(deps)* Bump tokio from 1.41.0 to 1.41.1 (#120)
- *(deps-dev)* Bump prettier from 3.3.3 to 3.4.1 (#127)
- *(deps)* Bump axum from 0.7.7 to 0.7.9 (#122)
- *(deps-dev)* Bump eslint-plugin-sonarjs from 2.0.4 to 3.0.0 (#123)
- *(deps)* Bump minijinja from 2.4.0 to 2.5.0 (#121)
- *(deps)* Bump backon from 1.2.0 to 1.3.0 (#119)
- *(deps)* Bump rsa from 0.9.6 to 0.9.7 in the cargo group (#115)
- *(deps-dev)* Bump @commitlint/cli from 19.5.0 to 19.6.0 (#125)
- *(deps)* Bump match-sorter from 7.0.0 to 8.0.0 (#124)
- Add gen config schema to release step
- *(deps)* Bump pyo3 from 0.23.2 to 0.23.3 in the cargo group (#146)
- *(deps)* Bump chrono from 0.4.38 to 0.4.39 (#140)
- *(deps-dev)* Bump globals from 15.12.0 to 15.13.0 (#137)
- *(deps-dev)* Bump eslint-plugin-promise from 7.1.0 to 7.2.1 (#135)
- *(deps)* Bump react and @types/react (#136)
- *(deps)* Bump react-dom and @types/react-dom (#138)
- *(deps-dev)* Bump vite from 5.4.11 to 6.0.3
- *(deps-dev)* Bump vite from 5.4.11 to 6.0.3
- *(deps-dev)* Bump lint-staged from 15.2.10 to 15.2.11 (#162)
- *(deps-dev)* Bump @types/node from 20.17.6 to 22.10.2 (#164)
- *(deps)* Bump home from 0.5.9 to 0.5.11 (#168)
- *(deps)* Bump ahooks from 3.8.1 to 3.8.4 (#166)
- *(deps-dev)* Bump eslint-plugin-react-refresh from 0.4.14 to 0.4.16 (#163)
- *(deps)* Bump async-openai from 0.24.1 to 0.26.0 (#167)
- *(deps-dev)* Bump prettier from 3.4.1 to 3.4.2 (#165)
- *(deps)* Bump thiserror from 1.0.69 to 2.0.7 (#170)
- *(deps)* Bump garde from 0.20.0 to 0.21.0 (#217)
- *(deps)* Bump serde from 1.0.216 to 1.0.217 (#216)
- *(deps)* Bump reqwest from 0.12.9 to 0.12.11 (#215)
- *(deps-dev)* Bump eslint-plugin-react-hooks from 5.1.0-rc-fb9a90fa48-20240614 to 5.1.0 (#200)
- *(deps-dev)* Bump @vitejs/plugin-react-swc from 3.7.1 to 3.7.2 (#201)
- *(deps-dev)* Bump typescript-eslint from 8.14.0 to 8.19.0 (#214)
- *(deps)* Bump openai from 4.72.0 to 4.77.0 (#203)
- *(deps)* Bump @uiw/codemirror-themes from 4.23.6 to 4.23.7 (#199)
- *(deps-dev)* Bump husky from 9.1.6 to 9.1.7 (#227)
- *(deps-dev)* Bump eslint-plugin-unicorn from 56.0.0 to 56.0.1 (#224)
- *(deps)* Bump glob from 0.3.1 to 0.3.2 (#220)
- *(deps)* Bump axum-streams from 0.19.0 to 0.20.0 (#221)
- *(deps-dev)* Bump eslint from 9.14.0 to 9.17.0 (#225)
- *(deps-dev)* Bump @types/node from 22.10.2 to 22.10.5 (#228)
- *(deps)* Bump @radix-ui/react-switch from 1.1.1 to 1.1.2 (#226)
- *(deps)* Bump async-trait from 0.1.83 to 0.1.85 (#223)
- *(deps)* Bump reqwest from 0.12.11 to 0.12.12 (#222)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 20 updates (#234)
- *(deps-dev)* Bump vite from 5.4.10 to 5.4.12 in /web-app in the npm_and_yarn group across 1 directory (#286)
- *(deps)* Bump the prod-npm-minor-dependencies group with 13 updates (#278)
- *(deps)* Bump the npm_and_yarn group with 2 updates (#285)
- *(deps-dev)* Bump eslint-config-prettier from 9.1.0 to 10.0.1 in the dev-npm-major-dependencies group (#281)
- *(deps-dev)* Bump vite from 5.4.10 to 5.4.12 in /web-app in the npm_and_yarn group across 1 directory (#292)
- *(deps)* Update pnpm to version 9.15.4 in package.json files
- *(deps)* Bump openssl from 0.10.68 to 0.10.70 in the cargo group across 1 directory (#316)
- *(deps)* Bump react-router-dom from 6.28.0 to 7.1.5 in the prod-npm-major-dependencies group across 1 directory (#317)
- *(deps-dev)* Bump vite from 5.4.14 to 6.0.11 in the dev-npm-major-dependencies group (#295)
- *(deps)* Bump react-router-dom from 6.28.0 to 7.1.3 in the prod-npm-major-dependencies group (#280)
- *(deps)* Bump the prod-npm-minor-dependencies group with 9 updates (#325)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 13 updates (#326)
- Implement workflow consistency tests (#291)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group across 1 directory with 4 updates (#366)
- *(deps)* Bump @tanstack/react-query from 5.66.0 to 5.66.5 in the prod-npm-minor-dependencies group (#358)
- *(deps-dev)* Bump the dev-npm-major-dependencies group with 2 updates (#400)
- *(deps)* Bump react-markdown from 9.0.3 to 10.0.0 in the prod-npm-major-dependencies group (#399)
- *(deps-dev)* Bump the dev-npm-minor-dependencies group with 7 updates (#398)
- *(deps)* Bump the prod-npm-minor-dependencies group with 2 updates (#397)
- Deprecate onyx-public-releases and use onyx (#419)

### 🚜 Refactor

- Remove unused GitHub workflows
- Update .cargo/config.toml
- Update Rust Toolchain Target and install script
- Simplify release binary naming convention
- Remove unused GitHub workflows and update installation script
- Add changesets workflow for release.yaml
- Update .cargo/config.toml and Cargo.toml
- Update release.yaml and README.md
- Remove unused GitHub workflows and update installation script
- Update installation script URL in README.md
- Update CI workflow to include pull request types and concurrency
- Update CI workflow
- Simplify CI workflow steps for formatting and linting
- Release.yaml to improve concurrency and cancel-in-progress behavior
- Avoid using sudo
- Dont override config file if it has already existed
- Ensure that install dir work
- Simplify condition checks and improve code readability (#83)
- Shorten the binary name
- Remove deprecated python setups
- Remove create-cache workflow and update release workflow for tauri and CLI builds
- Update changelog path in release workflow
- Unify arrow version and error handling inside connector (#349)
- [**breaking**] Change naming of some primites (steps, warehouses -> tasks, databases) (#401)
- Add alias to ensure backward compatability

### 📚 Documentation

- Update readme for internal setup and instructions (#15)
- Update theme and get started documentation (#19)
- License onyx to agpl v3
- Add explanation for protoc and remove windows
- Core workflow (#30)
- Add content to Ollama and Open AI (#36)
- Doc improvements (#52)
- Edit command for quickstart
- Update instruction
- Enhance quickstart documentation and refresh brand assets (#58)
- Update readme
- Create first draft of guide on contributing to documentation
- Add docs for the different workflow types
- Update basic command list to indicate you have to be in project repo
- Update config docs to new config style
- Update installation command
- Add CLI shortcut and command references (#149)
- Update welcome documentation titles and links
- Add workstation setup guide and beginner resources
- Restructure beginner resources and add new guides
- Add pull request template
- Add pull request template
- Update pull request template to enhance test plan and checklist sections
- Refine pull request template by removing unnecessary header
- Add release guideline (#218)
- Update agents and semantic model documentation with configuration and usage examples (#303)
- Split readme into two
- Add contributing docs
- Update banner
- Optimize image
- Correct reference to onyx
- Clean up readme to have only quickstart and intro
- Fix formatting in README.md for better readability
- Reset CHANGELOG

### 🎨 Styling

- Apply auto format to code
- Apply clippy fix
- Apply fmt fix

### ⚙️ Miscellaneous Tasks

- Update README
- Misc on error message and prompt
- Hardcode db connection string
- Improve output format prompt
- Cleanup println
- Add codeowners and precommit config
- Check build, run test and lint format
- Add change detection
- Use change detection in onyx ci
- Add continuous release workflow and configuration
- Add build frontend assets
- Lock pnpm ver
- Minor improvements to ci and release workflow
- Add auto release workflow
- Replace action setup of pnpm with yaml string
- Add package json file
- Temporarily disable cross compilation
- Use latest ubuntu for better version of protoc
- Use latest ubuntu for better version of protoc
- Remove cargo build in ci because it might takes too long
- Run cargo build with warnings only
- Treat warning as warnings
- Allow build to succeed with warnings
- Demo
- Temporarily cutdown some steps for CI because it takes too long
- Add concurrency control for release step
- Relax tagging scheme so more tags can be grouped together
- Set gh token for github public release
- Adjust concurrency key
- Set github user name for a successful tag
- Use a different cache key for better hit
- Fmt
- Bump version and push tag using formal actions
- Separate public release into another workflow
- Update release event type to "published"
- Update buffered
- Format code
- Fmt
- Replace public release job
- Temporarily allow edited event to trigger build
- Allow workflow public release to run manually
- Set owner and repositories in app token
- Ignore db file
- Return is_human and created_at
- Stream question answer object
- Cargo
- Stream question first
- Anchor sql look up in data path (#59)
- Sort by updated_at
- Bump version
- Use 3rd party action for some dependencies
- Cache pnpm
- Change repo name for onyx core to onyx
- Bump version
- Fix syntax error in release dep install
- Run db migration at startup (#66)
- Add linters to commit stage
- Fix db location, reduce streaming delay (#71)
- Remove fake streaming (#74)
- Enable cargo check to run on main
- Fix missing repo token
- Bump version
- Add dist/.gitkeep
- Bump version
- Bump version
- Add more events to automatically trigger release
- Turn off windows release for now
- Bump version
- Unify release into one job
- Onyx run should just run the query (#110)
- Change all to ref_name
- Bump version
- Remove reference to example config
- Add clear cache workflow and ignore docs folder
- File relative comment
- Fmt
- Bump version
- Install script for windows
- Allow onyx fmt to run on main branch again
- Dont run on main branch
- Bump version
- Upload json schema together with onyx bin
- Add a step to override old schemas
- Bump version
- Regenerate json-schemas
- Disable windows
- Render jsonl
- Change path from schemas to json schemas
- Remove gen config schema from ci
- Update workflows
- Bump version
- Fallback to default fetching behaviour for add and commit
- Bump version
- Bump version
- Bump version
- Bump version
- Bump version
- Bump version
- Move everything to examples
- Bump version
- Add cache for json schemas generation
- Remove files that might cause dirty state
- Trigger build
- Bump ver to test release
- Remove excessive steps
- Fix path for release artifacts
- Change order of runs and unify json schemas into prep release
- Unify into release-please
- Ignore ci when running on release branch
- *(main)* Release 0.2.0 (#173)
- Enable github release for release-please
- Release please should use draft
- Add some configs for release-please
- Adjust release-please
- Add tag true
- Try to sync configuration of release please with git cliff
- Bootstrap releases for path: .
- Change config for release please action
- Ignore label autorelease when running ci
- Match release manifest with current ver
- *(main)* Release 0.1.27 (#178)
- Add release-type rust
- Update condition for CI to run when autoreleasing
- *(main)* Release 0.1.27 (#179)
- *(main)* Release 0.1.28 (#180)
- Comment out update json schemas to save bandwidth
- Release please bootstrap sha
- *(main)* Release 0.1.28 (#185)
- *(main)* Release 0.1.29 (#186)
- *(main)* Release 0.1.30
- Change release please settings
- *(main)* Release 0.1.28 (#189)
- Remove draft setting or else tags wont be published
- Add missing artifacts dir
- *(main)* Release 0.1.29 (#191)
- Allow artifacts to be merged
- Ensure releases are executable
- Support passing tag to manual release
- Support passing tag to checking out
- Make tag input required for release workflow
- Prioritize tag input over ref name in release workflow
- Update job name to clarify binary compilation in release workflow
- Add GH_TOKEN environment variable for release tag declaration
- Fix output variable name for release tag in public release workflow
- Notification for releases (#195)
- *(main)* Release 0.1.30 (#194)
- Change slack action to use env
- Move generation of config schema to another job
- Update json schemas
- Remove unused deps (#197)
- Update dependabot schedule
- Add broken link check and typo check (#276)
- Remove unused dependencies (#274)
- Update broken link check to specify root directory and add .lycheeignore
- Update CI workflow to allow scheduled runs and set dependencies for typos and links jobs
- *(main)* Release 0.1.31 (#196)
- Update release workflow to create RELEASE_NOTES file before appending release notes
- Fix syntax error in public release workflow for tagging
- Fix output variable naming in public release workflow
- Update release workflow and fix formatting in package.json
- Enhance release workflow with unique identifier and improved tag description
- Update example
- Update example
- Rename install-desktop.sh to install_desktop.sh and add cleanup logic
- Update examples
- Increase lts version for node
- Remove commit-msg hook dependency on app web
- *(main)* Release 0.1.32 (#277)
- Add GH_TOKEN environment variable to release workflow for secure access
- Update base64 dependency and add cargo-workspace plugin to release configuration
- Remove unused library configuration from Cargo.toml
- Release main (#327)
- Update CI workflows and linting configurations
- Add Tauri dependencies installation step to CI workflow
- Temporarily disable all workspace due to disk space issue
- Trigger ci
- Update cargo commands to use workspace and re-enable JSON schema check
- Comment out Ubuntu build steps in release workflow
- Add cleanup step for Tauri build to reduce cache size
- Add checkout step in release workflow to ensure full repository history
- Update release configuration to include web-app component and version bump
- Add changelog to web-app folder
- Add node workspace plugin to release please config
- Update dependencies in package.json files
- Add .taurignore file to exclude specific directories
- Remove web-app configuration from release-please config
- Remove web app version
- Bring back outer changelog
- Remove default agents config, make defaults optional (#336)
- Housekeeping the docs and run cargo clippy fix for all codes (#342)
- Update packages
- Remove unused dependencies
- Release main (#343)
- Synchronise version
- Release main (#352)
- Revert changes to release script
- Release 0.1.35
- Ignore crates onyx desktop opensource folder
- Enable oss version to run ci and hide more files
- Define pr move and ignore push move
- Retain web app dist
- Hide dist file
- Use placeholder dist file
- Remove release guideline file (#367)
- Lock libduckdb sys
- Rename onyx community to onyx core
- Release main (#365)
- Release main (#375)
- Release main (#378)
- Sync version
- Update version to 0.1.38 for onyx and onyx-py packages
- Release 0.1.38
- Remove changesets job from prepare-release workflow
- Release 0.1.38
- Enable app signing for macos  (#371)
- Resync all changelogs
- Regenerate changelogs
- Bring back old changelogs and try another config
- Release main (#379)
- Revert to old configurations of release-please
- Comment out macOS certificate import and verification steps in release workflow
- Add "web-app" to the release-please configuration
- Remove "web-app" configuration from release-please setup
- Update pull request template to use comments for guidance
- Re-enable building CLI for ubuntu
- Reenable ubuntu build
- Upgrade deps
- Update cleanup command in CI workflow to target specific files
- Update cleanup command in CI workflow to remove all debug artifacts
- Add logging for tauri desktop app + unify logging folder  (#381)
- Resync pnpm lock yaml
- Enable releasing for minor versions
- Release 0.1.39
- Add support for 0.1.x branch in CI workflows
- Clean up old branches
- Update repository reference in public release workflow
- Change eval_type to type (#411)
- *(main)* Release (#387)
- *(release)* Comment out aarch64 target in workflow
- *(main)* Release (#418)
- Resync version
- Rename workflow
- *(experiment)* Push more branch to oss
- *(experiment)* Allow direct building from opensource repo
- Release 0.1.0
- Reset version
- Increase commit search depth to 1000 in release-please config
- Add debug logging for openai > reqwest (#429)
- *(main)* Release (#427)

<!-- generated by git-cliff -->
