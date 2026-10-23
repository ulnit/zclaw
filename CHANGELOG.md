# Changelog

## Unreleased

Changes landed after v0.6.5. No version bump yet: these are engine-only changes,
and releasing them as `0.6.6` would invalidate `mobile-ffi/Cargo.lock` and imply a
mobile app rebuild — that is a release decision, not bookkeeping.

### VACUUM leg activated — the holders scan is ported and wired (p3-holders)

The auto-maintenance VACUUM leg no longer fails closed. Upstream admits a VACUUM
only after proving no *other* process **and** no in-process sibling store holds
the state.db or a WAL sidecar (`hermes_state_maintenance.py:484-516`,
`hermes_state_holders`, #110054) — VACUUM plus its TRUNCATE checkpoint retires a
WAL generation a live sibling is still bound to, which is exactly the state where
every agent then refuses turns.

This port now proves it the same way, with both arms summed, never one alone:

- `foreign_state_db_holders` (Linux `/proc/*/fd` scan, `(st_dev,st_ino)` match)
  covers other processes; it skips our own pid, so it is paired with
- `in_process_state_db_holders` over a path-keyed live-store registry
  (`SqliteSessionStore::open` registers, `Drop` deregisters, `exclude=self
  .instance_id`) which covers a sibling store in this very process.

A non-empty sum records `vacuum_skipped_holders` (upstream's key — the earlier
`vacuum_skipped_reason` scaffold was removed; it was never an upstream field) and
skips; an empty sum admits the VACUUM, which now runs upstream's exact order
(`PASSIVE` fold-in → `VACUUM` → `TRUNCATE` checkpoint, so a 3 GB DB does not
leave a 3 GB `-wal` behind). Windows/macOS stay fail-closed *by design*: their
foreign arms (Restart Manager / psutil) are unported and return a placeholder
holder, so the leg skips there until slice4.

### Retry 529 as overloaded (P3-2, `1a45671`)

`retriable_status` in both `provider/openai.rs` and `provider/anthropic.rs` was
`408 | 429 | 500 | 502 | 503 | 504` — missing **529**. Upstream
`agent/error_classifier.py` classifies `overloaded = 503/529 → backoff`, so an
overloaded upstream returned a hard error instead of being retried. Added `| 529`
to both (the only two retry lists).

### Assistant reasoning is now persisted (P3-1, `f528e3f` + mobile-ffi `bd04e8d`)

Reasoning was received and then discarded: `ProviderResponse` and stream deltas
carry it (`openai.rs` already merges `reasoning_content`/`reasoning`/
`reasoning_details`), but `Message` had no field and the `messages` table no
column, so every turn's chain of thought vanished on store.

- `Message.reasoning: Option<String>` (`skip_serializing_if = Option::is_none`, so
  existing messages serialize byte-identically).
- `messages.reasoning TEXT`: new DBs via `CREATE TABLE`, existing DBs via an
  idempotent `ensure_reasoning_column` (`PRAGMA table_info` → `ALTER TABLE`),
  mirroring the `display_metadata` precedent. **Already-installed users'
  `state.db` upgrades losslessly.**
- Read back by both paths (`load_messages` and `load_messages_with_timestamps`,
  which feeds export/recap). `load_message_rows` returns the lightweight
  `MessageRow` and correctly needs none.

### Per-provider reasoning echo policy (P3-1b, `669afc5`)

Makes the stored reasoning usable for replay. New module `provider::reasoning_echo`
transcribes hermes' `_REASONING_ECHO_RULES` / `_REASONING_CARRIER_ROUTES` into
three tiers resolved from `(provider name, model, endpoint)`:

- **must-echo** — DeepSeek, Kimi/moonshot hosts, Xiaomi MiMo, Nous Portal
  `stealth/missingno`. Every assistant turn carries `reasoning_content`, padded
  with `" "` when there is no text; `""` is rejected by DeepSeek V4 (#17341), and
  omission is a proven HTTP 400 there.
- **strict** — Mistral, Groq, Cerebras, SambaNova, Tencent Hunyuan. Their
  `additionalProperties: false` schemas 400/422 on any reasoning key (#45655,
  #70233), so they never get one.
- **content** — openrouter, kilocode, nous, fireworks, novita, minimax, tokenhub,
  ollama-cloud, lmstudio, upstage; replays only when non-blank text exists.

Unknown routes resolve to `reasoning_content`, matching upstream
`_DEFAULT_CARRIERS = frozenset({RC})` ("every other route defaults to
`reasoning_content`, which is the de-facto standard"). That default is safe because of
the runtime rejection learner below — upstream ships both halves as one design, and so
does this port. *(This entry originally documented unknown routes as **off**, on the
grounds that ulnclaw lacked the learner that makes the upstream default safe. The
learner landed in P3-1d, which removed that justification; the deviation was then
corrected in `d5080e6` rather than left documented as intentional.)*

Tier precedence matches upstream too: **strict beats must-echo**. hermes returns
`pad and RC in carriers`, and a strict route's carrier set is empty, so the must-echo
pad is withheld there as well ("a strict route never gets a reasoning key, not even
the must-echo pad"). `d5080e6` corrected this port's original must-echo-first order,
which would have handed the pad to e.g. a deepseek *model* served through a groq
*host* and been rejected by groq's `additionalProperties: false` schema.

Applied to **assistant turns only**; the route is resolved once per request in both
the streaming and non-streaming paths.

**Fallback needs no reapply step.** hermes requires
`reapply_reasoning_echo_for_provider` after a fallback because it mutates a live
provider; ulnclaw's `build_fallback_provider` constructs a fresh provider per
entry and `name`/`model`/`endpoint` are immutable after construction, so the route
is derived at request time and follows the switch automatically.

**Anthropic is out of scope.** It replays reasoning as *signed* `thinking` blocks;
`signature` appears 0 times in `anthropic.rs` and `messages_to_anthropic` builds no
thinking blocks, so it replays nothing today and remains correct. hermes'
`agent/anthropic_thinking_policy.py` (signed replay + durable suppression) is a
separate, larger item.

### Runtime rejection learner (P3-1d, `0a7e01c`)

Completes P3-1b: a route whose schema actually rejects `reasoning_content` now
self-corrects after one 400, instead of that provider being permanently silent.
Ports hermes' `record_reasoning_field_rejection` /
`rejected_reasoning_carriers`.

Two things collapse versus upstream, both because of ulnclaw's shape rather than a
shortcut:

- **a `set` of carriers becomes one `AtomicBool`.** hermes can send three reasoning
  carriers and widens the drop when a non-standard one is named; ulnclaw's
  `ApiMessage` has exactly one (`reasoning_content`), so there is nothing to widen
  across.
- **no route map.** An `OpenAiProvider` instance *is* an immutable
  `(name, model, endpoint)` triple — the same tuple hermes uses as its route key —
  so per-instance state already is per-route state.

Learning triggers only when **all** of these hold, each guarding a way this could
otherwise break a working provider:

| Gate | Why |
|---|---|
| 4xx only | hermes `_status_ok`; 5xx/timeouts belong to the transient retry path |
| not a must-echo route | there a body naming the field is a *value* complaint ("must not be empty"), not a schema lacking it; dropping it would 400 every later tool turn (hermes: `fields -= {RC}`) |
| the field was actually sent | learning a field we never sent would silently disable reasoning for the session |
| the body **names** the field | an unnamed "extra inputs are not permitted" could be about any key |
| not already recorded | `record()` returns false on repeats, so recovery cannot loop |

Once learned, `effective_replay()` returns `Off` for that route, so later turns omit
the field without eating another 400 — hermes' "re-shapes `api_messages` without
them for the rest of the session".

The retry restarts the attempt budget via a labelled `continue 'outer` rather than an
inner `continue`: at `attempt == max_retries` an inner `continue` would fall out of
the loop and return `last_error = None`, i.e. the generic *"request failed after
retries"* — **swallowing the real 400** the recovery exists to fix. A test asserts
that string never appears.

**Scope: process-lifetime at landing; persistence added by P3-1e (`e5ad870`).**
hermes also writes rejections into the session's `model_config` so a resumed process
never re-learns with another 400. When this learner landed, ulnclaw's provider layer
sat *below* the session layer with no store handle and no session-level JSON KV
existed yet — recorded as owed, not scope-cut. The prerequisite
(`sessions.model_config` column + get/patch helpers, `6662a6e`) and the typed
storage (`e3e9830`) have since landed, and the wiring section below closes the
loop: learned rejections are persisted per session and seeded back on resume.

With this learner in place, unknown routes now send `reasoning_content` as upstream
does (`d5080e6`) — the learner is exactly what makes that default safe.

### Compression tail capped at 20% of the budget (`0117e54`)

Ports hermes' `TAIL_MAX_CONTEXT_FRACTION` (#104555, #117957). hermes documented the
failure this fixes: on a small-window model the "protected" verbatim tail *was* the
whole request, so every compaction summarised a few rows and reclaimed nothing.
ulnclaw's `keep_recent: 12` had the same bug from the other side — a fixed *message
count* is unbounded in tokens, so twelve tool-heavy turns can exceed any budget.

- `TAIL_MAX_CONTEXT_FRACTION = 0.20`: the verbatim tail may occupy at most 20% of
  `max_context_tokens`.
- `TAIL_MIN_MESSAGES = 3` floor (hermes `compressible_tail_cap = max(3, …)`): even an
  absurdly small budget keeps three messages, so the model never loses the turn it is
  currently answering. `keep_recent = 0` stays 0 — a caller that deliberately wants no
  tail isn't forced into the floor.
- `effective_tail_len()` walks newest-first, accumulating tokens until the next
  message would break the cap. `keep_recent` is now only the *requested* count.
- 🔴 **All three call sites** use it, including the `gateway_ws` `/compress` and
  gateway-slash `/compress` entry gates. Changing only the compressor's internals
  would leave those gates stricter than the internal decision (`keep_recent + 2` vs
  `effective_tail_len + 2`), so small-window sessions — the exact case the cap serves
  — would be rejected as "nothing to compress" before the compressor ever ran. Both
  pre-existing gateway tests pass unchanged.

An end-to-end test asserts the actual bug is gone: a 40 × 1000-token conversation
under a 10k budget must come back with **less than half** the tokens, fewer messages,
and the summary present.

The other half of that same hermes release — triggering compaction on
provider-reported usage instead of the chars/4 estimate — landed separately as
**P3-5b** below.

### Compaction triggers on real token counts (P3-5b, `4392b14`)

Ports hermes #104555: *"Compaction triggers on real token counts — provider-reported
usage, not bytes/4"*. `estimate_tokens` counts only message content and tool-call
arguments, so it systematically **under-counts** the real prompt: the system prompt and
the tool *schemas* never appear in `messages` at all. hermes #14695 puts that
incompressible floor at 20–30K tokens with 50+ tools — which means the old trigger
could sit under budget while the provider was already at its limit.

- `needs_compression()` is now **usage-anchored**: when a provider reading exists it is
  authoritative, and the estimate only covers messages appended since that reading
  (`anchor_len` snapshots `messages.len()` at send time, because the provider's usage
  describes *that* request — pairing it with whatever the loop appended before the
  response arrives would mis-anchor). With no reading yet it falls back to the estimate.
- `note_response_usage()` is called on **every completed response**, not only when
  usage is present: a response without usage is itself the signal that this provider
  omits it (hermes `note_usage_less_response`, #2153), and it is what consumes the
  post-compaction latch.
- `note_compaction_ran()` arms that latch and discards the reading, which described a
  transcript that was just replaced. 🔴 Without it the next iteration re-compresses on
  stale numbers — the death spiral hermes fixed in #4750/#2153.
- **Anti-thrash breaker**: two provider-confirmed *ineffective* compactions stop
  auto-compression (at landing: for the rest of the run — since superseded by P3-5c's
  recovery window below, and by P3-5d the state also survives across turns). When the
  incompressible floor alone exceeds the threshold, shrinking messages can never help and
  every pass is wasted money and lost context (#14695). A real reading back under the
  threshold resets it.
- The ineffective verdict is judged **only** in `note_response_usage`, never in the
  trigger check: hermes is explicit that the trigger runs twice per turn against two
  different measures (rough preflight vs real post-response, #36718), and the rough one
  can dip below the threshold and clear the strike every turn, re-opening the loop.

**Preflight deferral — ported by P3-6 (`24959ec`).** hermes'
`should_defer_preflight_to_real_usage` lets a rough over-estimate wait one request for
the provider to adjudicate, and its docstring states the prerequisite — *"its overflow
error still triggers reactive recovery"*. That prerequisite is now built (P3-6 slices
1-3 below), so deferral is safe and lands with upstream's structure intact:
`needs_compression` keeps its original contract, the deferral is asked by the gate
layer (hermes `turn_preflight_gate.py:97`) via the new `defer_preflight`, and
usage-anchored figures never defer. While the estimate was still the only preventive
signal (before P3-6), deferring would have turned a preventive compaction into a hard
400 — which is exactly why it stayed unported until reactive recovery existed.

**Scope limit — cleared by P3-5d (`b567f76`).** `ContextCompressor` used to be constructed
inside `run()`, and `run_with_session` is called once per user turn, so the usage anchor
accumulated only within a single run and never survived across turns. hermes keeps it on
`agent.context_compressor`. The next section lifts the state to a per-session registry.

### Compression state now survives across turns (P3-5d, `b567f76`)

hermes keeps `agent.context_compressor` so the real-usage anchor, the post-compaction
latch and the anti-thrash strikes carry from turn to turn. ulnclaw rebuilt the compressor
inside every `run()`, so all three reset each turn — the usage anchor never accumulated
past one turn, and a breaker tripped in turn *N* was forgotten by turn *N+1*.

🔴 The faithful port is **not** "one compressor field on `Agent`": ulnclaw's `Agent` is
shared across sessions (one gateway runner serves every session id), so a single field
would leak one session's anchor — and its tripped breaker — into another session's turns.
The state is therefore keyed by session id, on the `steer_inboxes` precedent.

- `CompressorRegistry` = `HashMap<session_id, (last_used_epoch, compressor)>`, LRU-capped
  at 64. An `Agent` is long-lived (a gateway process runs for weeks) while sessions are
  unbounded, so an uncapped registry would be exactly the unbounded-growth class of bug
  P3-3 exists to fix. Eviction only costs that session a fresh anchor next turn — hermes'
  gateway likewise rebinds a fresh compressor on cache eviction.
- `CompressorGuard` returns the state on `Drop`, so every one of `run()`'s several
  `return Err` paths preserves it. The registry lock is held only for the map operations
  and **never across an await**, so one session's summarization call cannot block another
  session's compression.
- `acquire_compressor(None)` (a non-persisting run) neither reads nor writes the registry:
  its state dies with the guard instead of parking under a shared key where unrelated runs
  would inherit it.
- Cross-run safety reuses the existing `anchor_len.min(messages.len())` clamp: `messages`
  is rebuilt each run so a restored `anchor_len` can exceed the new length, and the clamp
  keeps the slice in bounds. `note_response_usage(…, request_len)` re-anchors on the real
  reading every turn, so any skew self-corrects within one turn.

Tests (4): state survives across runs for one session; 🔴 sessions do not share state
(s1's latch must not suppress s2's over-budget message); unkeyed runs leave the registry
empty; the registry LRU-evicts past the cap (oldest gone, newest kept, length == cap).

Verification (WSL/Linux): lib+bins `--profile test` 0 errors; the 4 new tests pass;
`agent::` 20/20, `context::` 27/27; full suite 2539 passed / 1 failed (known flaky
`reload_mcp_cancel_keeps_everything`) + 1 ignored; mobile-ffi 0 errors; rustfmt 0 regions
(HEAD baseline 0; the 2 pre-edit regions were both in the new code). `agent/mod.rs` is a
leaf file, so a whole-file fmt is safe here — module roots recurse into submodules.

### Anti-thrash breaker recovers instead of blocking forever (P3-5c, `7259b9d` + `d3741c9`)

Ports hermes #14694 in full. Until now a tripped breaker disabled auto-compression
permanently — but the tripped verdict was made against the transcript *as it existed
then*, and the conversation keeps growing. Without a recovery path the session never
auto-compacts again and rides into the provider's hard context limit.

**Storage half (`7259b9d`)** — two durable `sessions` columns, byte-for-byte upstream's
shape (`hermes_state_common.py:433-434`, accessed via `_write_session_column`, **not**
`model_config`): `compression_ineffective_count INTEGER NOT NULL DEFAULT 0` and
`compression_recovery_deadline REAL` (NULL = unarmed; disarming writes NULL, per
upstream's `normalized or None`). Clamped at both write and read (`max(0, …)`), empty
session id never writes, store failures are logged not raised (`_durable_write`
semantics), idempotent migration in `open()`.

**Recovery clock (`d3741c9`)** — `ANTI_THRASH_RECOVERY_SECONDS = 300.0` (upstream
`_ANTI_THRASH_RECOVERY_SECONDS`). After a tripped breaker stays blocked for one window it
allows **one probation probe** by dropping the strike count to 1; if that probe is
ineffective the very next verdict re-trips, so the worst case is one compaction attempt
per window — bounded, not thrashing. Faithful details:

- 🔴 **Wall clock, never monotonic, and durable.** The compressor gets rebuilt (P3-5d's
  LRU eviction past 64 sessions, or any restart). A process-local deadline restarts the
  wait on every rebuild, so a tripped session never earns its probe — the exact bug hermes
  fixed in #100185 ("a blocked messaging session never earned its probe and stayed blocked
  forever"). Tests pin this directly.
- `bind_guard` makes the **durable row authoritative** (#54923 "restart must not disarm",
  #69872): a rebuilt compressor inherits the armed/tripped guard instead of starting clean.
- Lazy arming on the first BLOCKED evaluation; a backwards clock jump re-arms so the wait
  never exceeds one window; an untripped guard disarms any pending clock.
- Blocked-then-refresh mirrors `_refresh_durable_guards`: when about to block, re-read the
  row, because a sibling agent on the same session may already have cleared it.
- Strike changes go through `record_ineffective_verdict` (persists only on change), so
  every trip/reset path in `note_response_usage` keeps the durable row in sync.
- In the agent loop `anti_thrash_blocked()` is evaluated **before** `needs_compression()` —
  while tripped, `needs_compression` returns false immediately, so only the clock can clear
  the breaker to let the probe through.
- `CompressionGuardStore` is a trait so `context/` does not depend on `session/`; the agent
  supplies `SessionCompressionGuard` (store + session id → the two columns).

Tests: 4 storage (round-trip incl. disarm-writes-NULL verified through the raw column,
clamping incl. a hand-edited negative row, empty/unknown session, lossless migration from a
DB stripped of the columns) + 7 clock (trip blocks and arms ~300s out / elapsed window
grants one probe and re-trips on failure / 🔴 rebuilt compressor inherits the guard and
resumes the SAME window / 🔴 a durable past deadline lets a rebuilt compressor probe
immediately / sibling clearing the row unblocks via refresh / backwards clock jump clamped
/ unbound compressor runs the clock in memory only — the correct degraded mode).

Verification (WSL/Linux): lib+bins `--profile test` 0 errors; `context::compressor` 24/24,
`context::` 34/34, `agent::` 20/20, `session::sqlite` 51/51; full suite 2550 passed /
1 failed (known flaky) + 1 ignored, count 2545+7=2552 ✓; mobile-ffi 0 errors; rustfmt 0
regions (HEAD baseline 0).

⚠️ Process note: the first four clock tests failed because the fixtures tripped the breaker
in memory and *then* bound an empty mock — `bind_guard` correctly overwrote memory with the
durable 0. The bug was in the fixtures, not the implementation (the durable row being
authoritative is the behaviour under test); fixed by seeding the durable row first, which
is production order.

### Session-level `model_config` JSON KV (`6662a6e`)

Prerequisite infrastructure, no behaviour change. `sessions.model_config TEXT`
column (idempotent migration) + `get_model_config` / `patch_model_config`
helpers, mirroring hermes `get_session_model_config_value` /
`patch_session_model_config`. Two consumers were blocked on this: P3-1e
(rejected-carrier persistence) and P3-5d (compressor cross-turn state).

### Rejected reasoning-route persistence — storage half (P3-1e, `e3e9830`)

`sessions.model_config` gains a typed reader/writer for the
`reasoning_rejected_carriers` key, byte-for-byte the hermes format:
`[[provider, host, model, [carriers]], ...]`. Read-back filters carriers
against a `KNOWN_REASONING_CARRIERS` whitelist and drops malformed rows
entirely (a corrupt blob can never inject an arbitrary carrier or a phantom
empty route). Storage only at landing — the wiring half is the next section
(`e5ad870`).

### Rejected reasoning-route persistence — wiring half (P3-1e, `e5ad870`)

Connects the P3-1d runtime learner to the storage above, completing hermes'
"a resumed process never re-learns with another 400" contract. The provider
layer sits *below* the session layer and holds no store handle, so the wiring
lives in the agent (which already holds both the store and the session id):

- `Provider` trait gains three defaulted methods (the `service_tier`
  precedent — every existing provider and mock compiles untouched):
  `endpoint()` (the route key's third component), `learned_reasoning_rejection()`,
  `seed_reasoning_rejection()`. `OpenAiProvider` implements all three;
  `ReasoningRejectionLog` gains `seed()` — deliberately distinct from
  `record()`, because seeding must never look like "newly learned this call"
  (`record()`'s true-on-first-write is the retry loop's gate).
- `reasoning_echo::host_of` is now public: the persisted route key must use
  the exact host normalization the echo rules match on, or a stored rejection
  could never be found again.
- `Agent.active_session` (set in `run()` when the session row is created)
  routes reads/writes to the right session — providers outlive sessions and
  are shared across them, so this is a pointer, not the persistence key.
- **Seeding**: the primary provider at session start, and every lazily built
  fallback via `build_fallback_provider` (both `get_or_try_init` call sites go
  through it). Match is the exact `(provider, host, model)` triple — a
  rejection learned on one model never silences reasoning on another.
- **Persisting**: hooked at the `call_with` boundary on both paths
  (non-streaming success, stream establishment) — the learner fires inside the
  provider's retry loop, so the call boundary is where the agent can observe
  it. Hot-path cost is one relaxed atomic load. Merge-not-replace (same route
  updates in place, other routes survive, hermes `patch_session_model_config`
  semantics); unlearned providers never write; store failures warn and never
  fail the turn.

Tests (4): persist→seed round-trip asserting the stored key shape field by
field; exact-route matching (different model/host stays armed); merge
semantics (two routes coexist, re-persist doesn't duplicate); unlearned
provider writes nothing.

Verification (WSL/Linux): lib+bins `--profile test` 0 errors; the 4 new tests
pass; provider 63/63, agent 16/16, session 81/81, anthropic 12/12; full suite
2535 passed / 1 failed (known flaky) + 1 ignored; mobile-ffi 0 errors; rustfmt
0 new regions (openai.rs keeps its 5 pre-existing ones — no whole-file fmt,
per the standing rule).

⚠️ Process note: a stray recursive `rustfmt` on the module root
(`provider/mod.rs`) reformatted two files this change never touched
(`anthropic.rs`, `openai.rs` — fmt recurses through `pub mod` children). Caught
by the numstat audit (unexpected-file count ≠ 0), reverted, and the openai.rs
edit re-applied. Lesson recorded in the parity skill: only fmt leaf files.

### Session-store auto-maintenance: `auto_prune` on by default (P3-3, `75b1dcb` + `cac43d5` + `13f91d8` + `257f57c` + `b4242f9`)

Ports hermes #54189 in full: without automatic cleanup `state.db` grows without
bound (multi-GB installs reported within weeks upstream). User-authorized —
the default is destructive by nature, so it was flipped only after every
safety clause upstream has was ported and test-pinned first.

- **① Pinned exclusion (`75b1dcb`) — fixed a pre-existing data-loss bug.**
  Upstream `_prune_filter_where` excludes pinned rows by default ("Pinned is a
  durable keep flag"); ulnclaw's `where_clause()` had no pinned clause at all,
  so even the *manual* `sessions prune` could delete pinned sessions.
  `PruneFilters.include_pinned` (default false) now appends
  `COALESCE(s.pinned, 0) = 0`. The upstream pinned-tail CTE (unpinned
  continuations of a pinned compression ancestor) rides with the
  compression-lineage port — the port has no lineage handling at all yet;
  `pinned = 0` matches upstream default for every non-lineage case.
- **③+⑤ `sweep_orphaned_sessions` + `freelist_ratio` (`cac43d5`).**
  cron/kanban/subagent/one-shot CLI rows never stamp `ended_at` when their
  process dies, and prune only deletes ended rows — without the sweep they are
  immortal. Sweep *closes* them (`end_reason = 'startup_orphan_reap'`,
  resumable, messages kept). Faithful details: `started_at` AND last-activity
  must both precede the cutoff (`started_at` protects fresh branch/compression
  children with copied-old activity timestamps); sources are a whitelist
  (`AUTO_PRUNE_STALE_OPEN_SOURCES` = cli/cron/kanban/acp/api_server/subagent/
  tool/recovered, verbatim from `hermes_state.py:472` — messaging/UI sources
  have their own lifecycle owners, unknown sources fail closed, #60609);
  first-reason-wins (the UPDATE re-asserts `ended_at IS NULL`).
  `respect_gateway_heartbeats` is not ported because upstream's auto-prune
  call site passes `False` and this port has no heartbeat table — no gap.
  `freelist_ratio()` = `freelist_count/page_count` for the VACUUM gate.
- **④ `[sessions]` config block (`13f91d8`).** Keys and defaults transcribed
  from `config_defaults.py:2264`: `auto_prune = true` (🔴 NOT the `False` in
  `gateway/run.py`'s `.get(...)` — that is only a missing-key fallback),
  `retention_days = 90` (i64; negative skips maintenance — a future cutoff
  would match every ended session), `vacuum_after_prune = true`,
  `min_vacuum_interval_days = 30`, `min_interval_hours = 24`.
  `min_vacuum_freelist_ratio` stays a module constant
  (`AUTO_VACUUM_MIN_FREELIST_RATIO = 0.25`) — upstream never exposes it as a
  key. `auto_archive`/`auto_archive_days`/`show_subagents` are separate
  unwired features, NOT YET PORTED (documented in-code). 🔴 Distinct from
  `CheckpointsConfig`, which has same-named `retention_days` (7) for
  filesystem checkpoints; both structs cross-reference each other in docs.
- **⑥ `maybe_auto_prune_and_vacuum` orchestrator (`257f57c`).** Idempotent,
  never raises (errors land in `AutoMaintenanceReport`, panics caught):
  negative retention skips; a non-blocking cross-process lock
  (`<db>.auto-maintenance.lock`, `process_ctl::try_lock_exclusive` =
  flock/LockFileEx) makes concurrent startups skip instead of racing the
  interval check; 24h throttle in `state_meta` (`last_auto_prune`/`last_vacuum`,
  corrupt stamp = proceed, hermes `_seconds_since`); **prune first, sweep
  second** so a just-closed orphan gets a full retention window (test-pinned);
  throttle recorded even when nothing was pruned. VACUUM triple gate fully
  wired (`vacuum` + pruned>0 + interval + freelist ratio). 🔴 The VACUUM leg
  itself **fails closed**: upstream only rewrites after proving no sibling
  process or in-process generation holds the store/WAL (`hermes_state_holders`:
  /proc scan + Windows Restart Manager, #110054); this port has no holders
  scan, so quiescence is never provable and the leg always skips with
  `vacuum_skipped_reason` recorded. Manual `optimize_storage()` remains the
  explicit path; porting the holders scan activates the leg.
  *(Superseded — see "VACUUM leg activated" below: the holders scan has since
  been ported and the leg is now live.)*
- **Startup wiring (`b4242f9`).** `run_session_auto_maintenance(home, config)`
  runs once per process from `main()` before dispatch (covers every
  subcommand) and again per profile inside `build_gateway_stack` (hermes
  `_for_each_served_profile`: a multiplexed secondary profile has its own
  state.db AND its own `sessions` config). `spawn_blocking` + its own store
  open (thread-bound connections), gated on `config.sessions.auto_prune`,
  never fatal. A bin-level test proves the config is consumed: off → no store
  is even opened; on → maintained + throttled + idempotent.
- **🔴 Also fixed: the bin target was broken on the branch since P3-1
  (`6953d17`).** Six `Message` literals in `main.rs` lacked the `reasoning`
  field (`cargo check --bins` = 6× E0063). Invisible because every P3-1
  verification ran `--lib`, which never compiles the bin. All six are locally
  synthesized system notes → `reasoning: None`; the import path stays
  symmetric with export.rs (which never writes the key).

Verification (WSL/Linux): full lib suite 2531 passed / 1-2 failed (known
flakies `reload_mcp_cancel_keeps_everything`, `test_x_search_settings_editor`)
+ 1 ignored; `session::sqlite` 47/47, `session::filters` 7/7, `config::` 17/17,
`auto_maintenance` 6/6 + 1 wiring test; `cargo check --bins [--profile test]`
0 errors; mobile-ffi 0 errors; rustfmt 0 new regions; every diff pure addition.

### Reactive context-overflow recovery (P3-6, `1671507` + `a59e230` + `01b914f` + `24959ec`)

A provider verdict of "your context does not fit" used to be indistinguishable from any
other failure: `AgentError` flattened everything into `Provider(String)` and the agent
loop fed every error into `failover()`, burning a fallback provider on a problem that
only compression can fix. Four slices, in dependency order:

1. **Detection (`1671507`, `src/provider/error_class.rs`).** Port of hermes
   `agent/error_classifier.py`, scoped to the compress-vs-failover decision. 🔴 The
   pattern lists are not the contract — the ORDER is: `max_tokens` loads both ways
   (GPT-5's "Unsupported parameter: 'max_tokens'" is a request-shape rejection,
   hermes:1186); oMLX memory ceilings say "reduce context length" but measure bytes —
   compression cannot lower a prefill peak (#52261) → Overloaded; empty-response
   advisories and encrypted-reasoning replay rejections never compact (:377, :1157);
   Bedrock's 429 "Too many tokens" is a rate limit (:144). The two upstream chains
   differ — HTTP 400 (`_400_TAIL_RULES`) puts overflow BEFORE rate-limit/billing,
   status-less (`_MESSAGE_TAIL_RULES`) after — and are implemented separately rather
   than flattened. Matching semantics verbatim: lowercase once + substring.
   NOT YET PORTED (recorded, not silently trimmed): the large-session heuristic
   (needs approx_tokens/context_length/num_messages context), provider-scoped native
   code tables (#70414), plugin/profile/MoA verdict hooks, the other ~20
   `FailoverReason` variants, `_FREE_TIER_REFUSAL_PATTERNS` (unread lists are not
   invented; free-tier refusals fall to Other). 19 table tests.
2. **The verdict stays on the type (`a59e230`).** `AgentError::ContextOverflow(String)`
   + `is_context_overflow()`; the openai terminal-error path classifies with the
   structured `code` preserved (`ApiErrorDetail.code`, previously `dead_code` — a proxy
   may flatten the wording away but `context_length_exceeded` still means overflow).
   Non-overflow errors keep their exact shape and text. 4 mock-server e2e prove the
   WIRING (retry loop → learner → terminal path → variant), not the classifier.
3. **The loop uses it (`01b914f`).** `run`'s Err branch: overflow → reactive
   compaction → `continue` (retry recomputes `request_len`/`api_started` for the NEW
   transcript, or the usage report would pair with the pre-compaction snapshot and
   poison the P3-5b anchor). Deliberately bypasses `needs_compression`, the latch and
   the anti-thrash clock — upstream's overflow handler "compacts reactively in every
   case" — but is bounded to ONE attempt per run: a second overflow means compression
   cannot satisfy this provider, and the real error surfaces. Preflight and reactive
   paths share one `compact_messages` so both arm the same #4750 latch.
   `api_request_error` hook still fires first. 3 agent e2e: call sequence is exactly
   [conversation, summary, retry]; second overflow surfaces as-is; 🔴 a plain 500
   triggers ZERO compaction (without this guard the branch could destroy context on
   every failure).
4. **Preflight deferral (`24959ec`).** With recovery in place, a rough over-estimate
   waits one request for the provider's real usage — upstream's structure: the
   deferral lives in the GATE (`defer_preflight`, hermes `turn_preflight_gate.py:97`),
   never folded into `should_compress`/`needs_compression` (folding it broke 3
   pre-existing tests — restored verbatim by restoring the structure, and
   `p36_deferral_is_not_folded_into_needs_compression` pins the separation). Anchored
   figures never defer; `provider_omits_usage` never defers (#2153). 7 tests.

Verification: 29 new tests (19+4+3+7 → p36_/error_class suites 10+19); full suite
2583 passed / 1 failed (known flaky) +1 ignored; lib+bins+mobile-ffi 0 errors;
rustfmt: all touched leaf files 0 new regions (openai.rs 5 == HEAD 5).

## v0.6.5 — 2026-10-16

### Tool-naming alignment with hermes-agent v0.22.2 (P1 + P2a)

hermes-agent renamed several core tools in the v0.22.0 "Threshold Release"
(`todo` → `todo_list`, `cronjob` → `cronjob_manage`, `process` → `process_manage`)
and consolidated the preview tool into a `desktop_ui` toolset as
`desktop_preview`. ulnclaw now advertises the same canonical names.

- **Legacy aliases, not a breaking rename.** `LEGACY_TOOL_ALIASES` +
  `canonical_tool_name()` resolve the old names at every dispatch seam
  (`get`/`has`/`dispatch`), so old sessions and saved transcripts keep working.
  Aliases never enter the registry, so `definitions()`/`names()` advertise only
  the new names — the model never sees a duplicate tool, and no tokens are wasted.
  Mirrors hermes `model_tools._LEGACY_TOOL_ALIASES`.
- **`desktop_preview`** replaces `open_preview` (alias retained). Gains an
  optional `action` parameter (`enum=[open]`, default `open`); `close`/`read`
  are intentionally *not* stubbed — they need Electron bridge wiring that this
  build does not verify, so they are deferred rather than shipped hollow.
- **Model-visible text corrected:** the `terminal` tool description, the cron
  blueprint prompt seed, and `learn_prompt`'s tool list now name the new tools.
- **Deliberately unchanged** (verified by point, not blind-replaced): the kanban
  status literal `"todo"` and the usage-JSON field `"process"` share spellings
  with the renamed tools but are unrelated.
- **Scope note:** the remaining hermes-v0.22.2 tools (`set_theme`, `show_tip`,
  `gui_tour`, `setup_beat`, `manage_catalog`, `manage_connections`,
  `annotate_preview`, `read_window_below`, `drive_preview`, `desktop_project`,
  `browser_vault`, `browser_exec`) are desktop-gated in hermes
  (`platforms={"desktop"}` / `CLIENT_SURFACE_TOOLSETS`) and are **not** part of
  the mobile-ffi surface (which registers only the 7 phone-safe families), so
  they are out of scope for this release. The parity matrix body remains a
  v2026.8.3 audit; only the tool-naming layer tracks v0.22.2 here.
- **Tests:** `tools::tests` (alias mapping, idempotency, aliases-never-leak-to-schema,
  dispatch-via-alias, disabled-toolset-blocks-alias, error-echoes-original-name)
  plus the existing desktop suite. Verified on Linux (the Unix-only upstream test
  target does not compile on Windows).

## v0.6.4 — 2026-10-05

### Outbound mTLS (client certificates)

- **New: `ULNCLAW_MTLS_*` egress identity.** Deployments behind an
  mTLS-fronted gateway (a control plane that authorises/meters calls) can now
  present a client certificate on outbound HTTPS instead of staying
  unauthenticated. `ULNCLAW_MTLS_CERT` (+ `_KEY` when the key is not embedded),
  `ULNCLAW_MTLS_CA` for the gateway's private CA, `ULNCLAW_MTLS_TRUST_BUILTIN=0`
  for pin-only trust, `ULNCLAW_MTLS_HOSTS` to scope the identity, and
  `ULNCLAW_MTLS_MIN_TLS` (1.2 default / 1.3). Resolved from the process
  environment or `~/.ulnclaw/.env` on every client build.
- **Fail-closed.** A certificate without a key, an unreadable PEM, an
  unsupported min-TLS value, or an identity without `ULNCLAW_MTLS_HOSTS` is a
  hard error on the egress path — never a silent downgrade to anonymous TLS.
  (Presenting a client certificate to *any* host that asks for one is a
  credential leak, so the scope must be written down explicitly, `*` included.)
- **New CLI: `ulnclaw mtls status|probe|keygen|csr`.** `status` prints the
  resolved policy plus the leaf SHA-256 fingerprint (never key material);
  `probe <url>` contacts the endpoint with and without the identity (the
  anonymous arm also sends one `GET /`, because a TLS 1.3 server may complete
  the handshake and only verify the certificate afterwards) and reports
  `enforced` / `not-required` / `rejected` / `identity-missing` with a `reason` —
  evidence, not an assumption — and states whether the target is inside
  `ULNCLAW_MTLS_HOSTS` (`host_in_scope`), since the probe presents the identity
  on purpose while the running agent would not;
  `keygen`/`csr` bootstrap certificates via the system openssl.
- **Wired:** `src/provider/openai.rs` (OpenAI-compatible chat), `src/provider/anthropic.rs`,
  `src/stt.rs`, `src/tts.rs`; `outbound_tls::apply_env` / `client_for` for other
  reqwest call sites and `OutboundTls::rustls_config()` for raw-rustls paths.
- **Tests:** unit tests for scope matching, fail-closed validation, PEM loading
  and fingerprinting, plus `tests/outbound_mtls_test.rs` — a loopback rustls
  server that *requires* a client certificate: identity accepted, anonymous
  refused, probe verdicts checked in both directions.
- **Docs:** `docs/design/outbound-mtls.md` (design + operations),
  provider guides (en/zh), parity matrix note (this is a deliberate ulnclaw
  extension — the hermes parity baseline has no outbound client certificates),
  README feature row.

### Fixes found by the end-to-end run

- `--san` is now case-insensitive (`dns:` == `DNS:`), normalised to the openssl
  spelling before it reaches `openssl req`. The `--help` text and the design note
  both showed lowercase, so the documented example used to fail validation.
- Scope matching (`ULNCLAW_MTLS_HOSTS=*.example.com`) requires a label boundary:
  `evilinternal.example.com` is no longer matched by `*.example.com`, and the
  identity is therefore never presented to it (fail-closed).
- A rustls `CryptoProvider` is installed explicitly before building client/server
  configs (process-wide default plus the outbound-TLS entry point), so mTLS works
  in binaries that never initialise a provider themselves.
- The mTLS probe now distinguishes "handshake completed but the request was
  refused" (TLS 1.3 post-handshake certificate check, e.g. openssl's
  `CertificateRequired` alert) from "endpoint served us anonymously", which is
  the difference between `enforced` and `not-required`.


## v0.2.0 — 2026-08-10

Hermes v2026.8.3 parity milestone.

### Hermes feature parity

- **50+ built-in tools** — terminal/process, file read/write/patch/search,
  web search/extract, X search, video understanding, memory, todo, session
  search, clarify, skills, delegation (background semantics + persistent
  async registry), execute_code, cronjob, vision, image generation, video
  generation, desktop projects, Discord server tools, Feishu docs, Spotify,
  Yuanbao, cross-channel `send_message`, learning timeline, TTS, Home
  Assistant, kanban, tool search.
- **MCP client** — stdio + Streamable HTTP/SSE, OAuth 2.1 + PKCE, lazy
  servers, malware preflight, catalog one-click install, dashboard
  management with OAuth brokering.
- **Browser automation** — 12 `browser_*` tools over CDP, managed headless
  Chrome, Camofox, Browserbase/Browser Use/Firecrawl cloud sessions,
  hermes-grade SSRF guards, forced secret redaction.
- **Computer-use** — cua-driver backend, status/doctor, settings surface.
- **Secrets vaults** — command / Bitwarden Secrets Manager / 1Password
  sources with full hermes precedence semantics, TTL cache, AES-GCM
  encrypted Bitwarden cache, setup wizards, gateway sync action.
- **Skills sync** — device identity, pull/push/now, enable/disable, remote
  manifest, gateway pull/push actions.
- **Messaging** — 26-platform catalog with posture, loops with bounded
  retry, cross-channel messaging, Doctor env-value editor.
- **Plugins + shell hooks** — lifecycle hooks with consent gate, plugin
  install/enable/update, hook doctor.
- **OAuth** — device-flow login + account sync surfaces.
- **Egress firewall** — managed iron-proxy sandbox outbound proxy.
- **Monitoring** — gateway health/diagnostic events, OTLP export, redaction.

### Gateway

- OpenAI-compatible `/v1/chat/completions` + `/v1/responses` with SSE
  streaming, async `/v1/runs` with approval resolution, sessions API with
  per-session model lock, 100+ `/api` management endpoints.

### Desktop (Tauri 2)

- Chat UI with session management, settings dialogs, Doctor diagnostics
  (30+ panels), themes + fonts, quick entry, deep links, tray integration,
  native notifications, update checks, gateway log viewer, single-instance
  handoff, quit guard.

### Continuous delivery

- Tag pushes run GitHub Actions (`.github/workflows/release.yml`):
  Windows NSIS installer + macOS dmg (Apple Silicon and Intel) built
  with the core binary bundled inside, checksums published, assets
  attached to the release (P805).

### Verification

- 2399 lib tests passing; desktop `tsc --noEmit` + vite build green.

## v0.1.0

Initial development series (P1–P336): core agent loop, tool registry,
gateway foundations, CLI surfaces.
