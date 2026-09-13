# Context Surface Plan — token-bounded tool output for lightbrowse

Status: **P0 + P1 implemented** — `lightbrowse-core::reduce`, CLI `--max-tokens`, artifact store in `lightbrowse-memory::artifacts`, MCP interception + `artifact/*` tools (see §9 and §10); P2 (snapshot delta, lazy schemas) still proposal
Source of the mechanism: `codelocal-cloud/codelocal` (Go, Apache-2.0, v1.5.64) — `internal/contextsurface/*`, `internal/projectbrain/compiler.go`, `internal/mcphub/*`
Target: lightbrowse v0.5.2 (Rust workspace, ~12.6k LOC)

Goal: cut the tokens lightbrowse injects into an agent's context (snapshot trees, extracted
text, console/network traces, evaluate results) by 60–80% **without losing the ability to
recover the exact bytes** the agent might need later.

---

## 1. The mechanism we are porting (as actually implemented, verified from source)

Five pillars, none of them model-based — all deterministic Go code.

| # | Pillar | Where | What it does |
|---|--------|-------|--------------|
| 1 | Compact tool surface | `internal/mcpgateway/compact_tools.go` | 20 public MCP tools with merged `action`-routed ops, replacing 77 granular handlers |
| 2 | Lazy extension discovery | `mcpgateway` `mcp` tool + `internal/mcphub` | Extensions are searched/inspected/called on demand; search default `limit=8`, max 50; tool preview truncated when >100. Extension schemas never ship at session start |
| 3 | Bounded brain packet | `internal/projectbrain/compiler.go` | `ContextPacket{Budget, Truncated, Fingerprint}`; rules bucketed into mandatory/relevant lanes; fingerprint = sha256(brain fingerprint + lane/id/text of every item) → stale/delta detection |
| 4 | Model-free observation reducer | `internal/contextsurface/reducer.go` | Line-level scoring + token-budget selection + syntactic fallback; raw bytes stay behind `RawArtifactRef`; emits `OriginalTokens`/`ReducedTokens` |
| 5 | Reversible compaction | `internal/contextsurface/compaction.go` | Model-free checkpoint with markers referencing hidden item IDs; **required evidence can never be dropped** (`ErrCompactionMandatoryOverflow`); per-lane budget accounting |

### The reducer in detail (the part worth copying)

```
ReduceObservation(obs, maxTokens)
  maxTokens = clamp(maxTokens, 512, 4096)          # default 512
  kind = classify(kind | tool | operation)         # tests|build|compiler|git|lsp|browser|computer|terminal|agent|generic

  semantic branch:                                  # if any line scores >= 50
    score(line) = 1
      + 100 if error|failed|failure|panic|exception|fatal|conflict|denied|timeout|assert
      +  45 if warning|warn:|exit code|exit=
      + kind-specific keyword boosts (60..100)
      +   8 if line is in the first 2 or last 2 lines
    neighbors of a high-score line are floored to 35 (prev) / 30 (next)   # context guard
    greedy pick by (score desc, index asc) until token budget; then re-sort by index

  syntactic fallback (no semantic signal):
    head = 3/4 budget, tail = 1/4 budget
    marker = "… output pruned; raw artifact retained …"
    shrink until it fits

  out = ReducedObservation{id: "observation:"+sha256(kind,tool,op,text)[:16],
                           raw_artifact_ref, original_tokens, reduced_tokens, trust="observed"}
```

Two properties make this safe rather than lossy-and-hopeful:

1. **The ID is content-addressed and the raw bytes are addressable.** The model sees a bounded
   projection plus a handle; a missing detail is one tool call away, not lost.
2. **Mandatory evidence is a hard invariant.** Compaction refuses to fit rather than drop
   required items (`ErrCompactionMandatoryOverflow` instead of silent truncation).

Caveat recorded honestly: in the v1.5.64 snapshot, `ReduceObservation` has exactly one caller
(`internal/toolprogram/runtime.go`, a bounded declarative program runtime that appears not to be
wired to the public MCP path). So treat this as a **design**, validated by unit tests
(`reducer_test.go` asserts verbatim `go test` output reduces to <80 tokens), not as a shipped
end-to-end pipeline. We are exporting the idea, not the integration.

### External validation (grounded research, Google AI Mode, 2026)

- **Deferred schema loading** is now standard in Claude Code-style harnesses: preloading schemas
  for dozens of MCP tools costs 50k–134k tokens at session start; loading names only and
  fetching schemas on demand saves up to ~84% of that setup cost.
- **Differential snapshots**: send only the mutation delta after an action ("#ref12 changed
  Unchecked → Checked") instead of re-sending the whole tree; later steps approach zero token
  growth and keep the fixed prefix cacheable.
- **Subtree collapsing**: 100 structurally identical product rows → keep 3, collapse the rest
  into one metadata line.
- **Compact serialization**: emitting pruned accessibility trees as a compact YAML-ish form
  instead of verbose XML/HTML measured 51–79% fewer tokens for the same content.
- **Per-snapshot token budgets + off-context artifacts** are the recommended guard against
  infinite-scroll / huge-DOM blowups.

lightbrowse has none of these today; it caps raw HTML (`max_html_bytes = 2 MiB`) and snapshot
`max_nodes`, and truncates a few preview fields (`take(4000)`, `take(300)`), but every cap is a
*truncation*, never a ranked projection with a recoverable artifact handle.

---

## 2. Fit against lightbrowse (what we already have)

| Asset | Location | Why it matters here |
|-------|----------|---------------------|
| 37 MCP tools + ~470 lines of schema in `tools_schema()` | `crates/lightbrowse-mcp/src/lib.rs:1196` | Pillar 2 is high-ROI here: 37 tools with long descriptions are pure context tax |
| Single tool choke point | `crates/lightbrowse-mcp/src/lib.rs:205` `call_tool() -> Result<String,String>`, 28 `Ok(pretty(json!))` sites | One wrapper function bounds every MCP response — no per-site edits |
| Accessibility tree already structured | `crates/lightbrowse-core/src/snapshot.rs` (`SnapshotNode` has `uid`, `role`, `text`, `selector`, `bbox`, `children`, `parent`-ordered `nodes`) | Perfect input for a ranked projection; needs ancestor-guard, not a rewrite |
| Content-addressed-ish storage already exists | `crates/lightbrowse-memory/src/lib.rs` SCHEMA: `pages`, `blocks`, `cache`, `html_cache`, `runbooks` + `PRAGMA user_version` migration precedent | `raw_artifacts` is one more table + `user_version=2`; no new store to invent |
| `ask` + BM25 memory search | `lightbrowse-mcp` `ask`, `memory/search`; `lightbrowse-memory` | **Our improvement over CodeLocal**: artifacts are not dead blobs — they become searchable memory, so `ask_artifact` = ask over the stored raw payload |
| CLI/JSON surface | `crates/lightbrowse-cli/src/main.rs:396` (`text_preview` take(4000)) | Same reducer should back CLI `--json` output, not only MCP |

## 3. Decisions

| Pillar | Decision | Reason |
|--------|----------|--------|
| 1 — Compact tool surface | **Skip** | 37 → 1 dispatcher would break existing CLI/MCP contracts and agent habits for marginal schema savings; the real cost is descriptions, which Pillar 2 fixes |
| 2 — Lazy schema discovery | **Port (P2)** | Biggest single win: 37 tool schemas. Keep names + one-line summaries in `tools/list`; full JSON Schema behind `tool_inspect(name)` |
| 3 — Bounded brain packet | **Adapt, narrow** | No project brain here. Port only the *fingerprint* idea → page-state fingerprint for snapshot delta suppression |
| 4 — Model-free reducer | **Port (P0)** | Highest ROI, self-contained, testable in isolation |
| 5 — Reversible compaction | **Port (P1)**, reusing memory.db | Artifact store + refusal-to-drop-mandatory invariant |

Extensions CodeLocal does not have, which we should add:

- **Tree Context Guard** — CodeLocal reduces flat lines; snapshots are trees. Keep the ancestor
  chain (depth 2) plus siblings-of-a-kept-interactive-node so a retained `[102] button "Submit"`
  never arrives without its form/modal context. This is the #1 way this mechanism misleads a browser agent.
- **Snapshot delta** — fingerprint the page after each action; if the new snapshot's hash equals
  the previous one, return `"snapshot unchanged (fingerprint abc123)"` instead of the tree.
- **Artifact ↔ memory bridge** — push reduced projection *and* raw payload into `memory.db`, so
  `ask_artifact` reuses `ask` + BM25 instead of a bespoke reader.

## 4. Rust design

### 4.1 `lightbrowse-core::reduce` (new module; no new crate)

```rust
// crates/lightbrowse-core/src/reduce.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationKind { BrowserTree, PageText, ConsoleLog, NetworkTrace, Evaluate, Generic }

impl ObservationKind {
    pub fn classify(tool: &str, operation: &str) -> Self { /* snapshot|visual_snapshot -> BrowserTree, extract|navigate|page/current -> PageText, ... */ }
}

#[derive(Debug, Clone)]
pub struct ReduceConfig {
    pub max_tokens: usize,        // default 512, clamp [256, 4096]
    pub preserve_neighbors: bool, // default true
    pub tree_ancestor_depth: u8,  // default 2
    pub max_payload_bytes: usize, // default 10 MiB (artifact write cap)
}

#[derive(Debug, Clone)]
pub struct Reduction {
    pub id: String,                       // "obs_" + blake3(kind|tool|op|raw)[..16]
    pub kind: ObservationKind,
    pub payload: String,                  // what the model sees
    pub original_tokens: usize,
    pub reduced_tokens: usize,
    pub strategy: ReduceStrategy,         // Ranked | HeadTail | Passthrough | FingerprintOnly
    pub truncated: bool,
}

pub fn estimate_tokens(s: &str) -> usize;               // reuse CLI/character heuristic, one impl
pub fn score_line(line: &str, kind: ObservationKind) -> i32;
pub fn reduce_observation(raw: &str, kind: ObservationKind, cfg: &ReduceConfig) -> Reduction;
pub fn reduce_tree(tree: &SnapshotTree, cfg: &ReduceConfig) -> Reduction;   // ancestor guard lives here
pub fn fingerprint(bytes: &[u8]) -> String;             // blake3[:16]
```

`score_line` ports CodeLocal's keyword weights **verbatim** for parity, then adds our kinds:
browser-tree boosts for `disabled`, `hidden`, `aria-*`, `required`, `stale`, `not found`,
`dialog`/`modal`, `error`; console boosts for `error`, `uncaught`, `failed to load`,
`CORS`, `403/404/500`; network boosts for non-2xx status, `set-cookie`, redirect chains.

`reduce_tree` is the differentiator: score nodes, pick within budget, then **re-add ancestors and
immediate siblings of every kept interactive node**, then re-serialize in document order.

### 4.2 `lightbrowse-memory::artifacts` (new module in existing crate)

```sql
-- appended to SCHEMA, gated by PRAGMA user_version = 2
CREATE TABLE IF NOT EXISTS raw_artifacts (
  id           TEXT PRIMARY KEY,      -- "obs_<hash>"
  tool         TEXT NOT NULL,
  kind         TEXT NOT NULL,
  url          TEXT,
  content_type TEXT NOT NULL,
  bytes        BLOB NOT NULL,
  bytes_len    INTEGER NOT NULL,
  tokens       INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_raw_artifacts_created ON raw_artifacts(created_at);
```

```rust
pub fn put_artifact(&self, rec: &ArtifactRecord) -> Result<()>;
pub fn get_artifact(&self, id: &str) -> Result<Option<ArtifactRecord>>;
pub fn purge_expired_artifacts(&self) -> Result<usize>;   // default TTL 24h
pub fn artifacts_bytes(&self) -> Result<i64>;             // enforce hard cap (e.g. 500 MiB, LRU-evict)
```

Writes go through an `mpsc` channel to a background thread (a synchronous blob write inside the
CDP loop would add browser-action latency); `PRAGMA journal_mode=WAL` for reader/writer concurrency.
Raw payloads are also indexed into the existing `pages`/`blocks` path when they are text, so
`ask_artifact` is just `ask` scoped to an artifact URL.

### 4.3 `lightbrowse-mcp::surface` (new module; one wrapper, plus lazy schema)

```rust
// crates/lightbrowse-mcp/src/surface.rs
pub struct Surface { cfg: ReduceConfig, artifacts: ArtifactStore }   // Arc-backed

impl Surface {
    /// Wrap the single choke point in `call_tool`.
    pub async fn bound(&self, tool: &str, operation: &str, raw: String) -> String;
    //           ^ stores raw as artifact when reduced, appends the standard footer, returns payload
}

// new tools
"artifact/read"  { id }                  // exact raw bytes (optionally byte/line range)
"artifact/ask"   { id, question }        // reuses ask over the artifact's stored text
"artifact/list"  { limit, tool, kind }   // recent artifacts + token savings accounting
"tool/inspect"   { name }                // full JSON Schema on demand (Pillar 2)
```

`call_tool` change is ~5 lines: every `Ok(pretty(...))` return funnels through
`surface.bound(name, op, out)` once, before `return`. Every reduced payload ends with a fixed
footer, which is the guard against "the agent assumes a missing element does not exist":

```
[reduced 812→96 tokens · full output: artifact/read id=obs_9f2c1a77]
```

### 4.4 Snapshot delta (P1, after the reducer)

Store `fingerprint(kind, url, payload)` per (session, tab). In `call_tool("snapshot")`:
if the fingerprint is unchanged since the previous snapshot on that tab, return
`{"unchanged": true, "fingerprint": "…", "artifact": "obs_…"}` (≈15 tokens) and let the agent ask
for the tree explicitly. This is where multi-step automations stop growing linearly.

## 5. Phases and acceptance criteria

| Phase | Work | Acceptance criteria |
|-------|------|---------------------|
| **P0** (est. 3–4 d) | `core::reduce` (scoring, ranked selection, head/tail fallback, token estimate) + `reduce_tree` ancestor guard + tests | Unit parity tests mirroring CodeLocal's `reducer_test.go`; verbose `cargo test` output → <100 tokens; synthetic 2000-node tree → ≥60% token cut; **0** retained interactive nodes lose their ancestor chain or `selector` |
| **P1** (3–4 d) | `memory::artifacts` (schema v2, WAL, async writes, TTL/LRU), `mcp::surface::bound` wrapper, `artifact/read`, `artifact/ask`, footer marker | Every reduced MCP response carries a resolvable handle; `artifact/read` returns byte-identical raw (`assert_eq`); reducer adds <10 ms/call; write path adds 0 ms blocking; DB cap enforced; `ask_artifact` returns passages for a 5-query fixture |
| **P2** (2–3 d) | Lazy `tools/list` (names + one-line summaries), `tool/inspect`, snapshot delta, CLI `--json` uses the same reducer | Startup `tools/list` payload ≥50% smaller (37 schemas → summary list); unchanged-page snapshot ≈ ≤20 tokens; CLI output bounded identically for `snapshot`/`extract` |

Measurement harness (shared with `lightbrowse-mcp` tests + `memory-stats`):
a `--stats` counter of `original_tokens`, `reduced_tokens`, artifacts stored, HIT rate
(`artifact/read` calls ÷ reduced responses). Report it as the honest number, not a promise.

## 6. Failure modes and guards

| Risk | Guard |
|------|-------|
| Retained action element arrives without form/modal context | Tree Context Guard (ancestors + siblings), asserted in tests |
| Agent assumes pruned content does not exist | Mandatory footer telling it to `artifact/read`; `artifact/read` listed in `help` |
| Reduction of a page where everything scores 0 (no errors, no keywords) | Fall back to head/tail, never to empty |
| Chrome selector fragility if pruning mutates paths | Never mutate the tree in place; reduction is a *projection* — `selector`/`uid`/`bbox` come from the original node |
| SQLite contention / bloat | WAL + async mpsc writes, 10 MiB per-payload cap, 24h TTL, 500 MiB total cap with LRU eviction |
| Token estimate drift | One `estimate_tokens` implementation, calibrated against a fixture corpus; report `original_tokens`/`reduced_tokens` in every response for auditability |
| Reducer used on secret-bearing output (cookies, vault, `type` with password) | Never store artifacts for tools on a deny-list (`cookies`, `vault/*`, `login`, `type` with password fields); keep the existing redaction path upstream of the reducer |
| Silent regression in agent task success | Fixture-based end-to-end test: 4–6 recorded multi-step flows (login form, paginated list, infinite scroll, JS-heavy SPA) with fixed action sequences; the reduced surface must still let the scripted agent complete them |

## 7. What we deliberately do not do

- No model-based summarization (an LLM call to compress output costs more tokens than it saves
  and makes reduction non-deterministic).
- No project-brain/workspace indexing — that is CodeLocal's domain, not a browser tool's.
- No silent truncation without a handle anywhere in the codebase; every cap either keeps an
  artifact or explicitly reports `truncated: true` with the exact limit that applied.
- No changes to the CDP layer; the reducer is a pure function in `core` that the MCP and CLI
  surfaces call. `lightbrowse-cdp` is untouched, which keeps the security-sensitive code frozen.

## 8. Reference: portable invariants worth keeping

1. Reduction is **model-free and deterministic** — same input, same output, unit-testable.
2. **Handle, don't hold** — raw bytes live in a store; the model gets a bounded projection + id.
3. **Mandatory evidence cannot be dropped** — refuse rather than silently lose required items.
4. **Account for every call** — `originalTokens`/`reducedTokens` returned, aggregated, and exposed.
5. **Fallbacks are explicit** — `strategy: ranked | head_tail | passthrough | fingerprint_only`.
6. **The footer is mandatory** — the model must always know a reduction happened and how to undo it.

## 9. P0 results (implemented, measured)

Shipped in `lightbrowse-core::reduce` (pure module, no new dependencies) and wired into the CLI on
`fetch`, `extract --mode text` and `snapshot` via `--max-tokens` (default 1000 tokens, `0` disables).

Local gates matching CI: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` — all green, plus 16 unit tests in `reduce.rs` (ranked selection, neighbour guard,
head/tail fallback, over-long line clipping, budget enforced after join, content-addressed id, tree ancestor
guard, signal-free tree fallback, passthrough cases).

End-to-end measurements against the built CLI (tokens ≈ output bytes / 4):

| Command | Before | After | Saved |
|---------|--------|-------|-------|
| `fetch https://en.wikipedia.org/wiki/Rust_(programming_language)` | 10,772 tok | 910 tok | **92%** |
| `fetch https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html` | 6,261 tok | 911 tok | **86%** |
| `extract --mode text` (same Wikipedia page) | 21,476 tok | 898 tok | **96%** |
| `snapshot https://doc.rust-lang.org/std/index.html --max-nodes 900` | 903 nodes / 36,911 tok | 98 nodes / 567 tok | **98.5%** |

`--max-tokens 0` returns the complete output, so the projection is always optional. Every response that
dropped content carries `reduction {strategy, original_tokens, reduced_tokens, saved_tokens, id, marker}`.

Not yet done (P1/P2): artifact handles (`artifact/read`, `artifact/ask`) so a reduced projection can be
expanded again, snapshot fingerprint deltas, MCP-side interception, and lazy `tools/list` schemas.

## 10. P1 results (implemented, measured)

### Artifact store — `lightbrowse-memory::artifacts`

`raw_artifacts` (schema `user_version = 2`, WAL, 5s busy timeout) shared with the page cache in
the same SQLite file. Writes are **asynchronous**: `put` enqueues to a writer thread that owns its
own connection, and a pending map serves reads immediately after a write (read-your-writes) until
the row is durable. Expiry is 24h by default, enforced on open, hourly in the writer, and via
`purge_expired`; a 512 MB total cap evicts oldest-first (checked on every write against a running
total); a payload over 10 MB is **refused** (`put` returns `false`) rather than truncated, so a
caller never hands the model a lossy answer with no handle.

Schema versioning stayed safe: `ensure_schema` is shared by `MemoryStore` and the artifact writer,
so the `user_version = 1` legacy `login-*` scrub still runs even when the artifact store opens the
file first (asserted by a test that seeds a pre-migration DB).

### MCP interception

`McpServer::bound_output` wraps the single `tools/call` choke point. Rules:

- **Allow-list only** (`navigate`, `extract`, `ask`, `snapshot`, `visual_snapshot`, `page/current`,
  `evaluate`, `search`, `research`, `memory/search`). `vault/*`, `login`, `type`, `fill_form`,
  `cookies`, `runbook/*`, `screenshot` and `artifact/read` are never reduced or stored.
- **Recoverable-or-nothing**: the full payload is stored as an artifact *before* the reduced
  response is returned; if storage fails or the payload exceeds the cap, the full response is
  returned unchanged.
- Projection keeps **valid JSON**: snapshot trees are pruned structurally (ancestor guard intact),
  oversized arrays are collapsed to their leading items plus a marker, and long string fields are
  ranked-projected. The `reduction` object carries strategy, token counts, artifact id and marker.
- Disabled with `--max-tokens 0` or `$LIGHTBROWSE_MAX_TOKENS=0`; default **1000**.

### Measured end-to-end (MCP over stdio, `--max-tokens 1000`)

| Tool call | Before | After | Saved |
|---|---|---|---|
| `extract` (Wikipedia article, mode=text) | 41,947 tok | 1,062 tok | **97%** |
| `extract --mode links` (rust std index, 900 links) | 11,508 tok | 529 tok | **95%** |
| `snapshot` (rust std index, 900 nodes) | 81,825 tok | 487 tok | **99%** |
| `navigate` (Wikipedia article) | 1,078 tok | 929 tok | 13% |

`artifact/read` returned the byte-identical pre-reduction payload (recorded `tokens = 1078`),
`artifact/ask` returned ranked passages, `artifact/list` reported the store totals. 13 new unit
tests across the two crates (store roundtrip/read-your-writes/cap/TTL/LRU/reopen/versioning,
allow-list, tree pruning with ancestors, array collapsing, payload roundtrip, no-op paths).

### Still open (P2)

- Snapshot fingerprint delta: an unchanged page should return ~15 tokens instead of a projection.
- Lazy `tools/list`: 37 tool schemas (~470 lines) ship at session start today; names + one-line
  summaries with full schema behind `tool/inspect` would cut startup context by ~50%.
- Array marker as a non-string element if a host rejects mixed arrays.
