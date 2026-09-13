# Changelog

All notable changes to lightbrowse are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.1] — 2026-09-13

Follow-up to 0.6.0: the reported savings now match the bytes on the wire, and `extract --mode text`
no longer pays for the article twice.

### Fixed

- **Token accounting matched neither the wire nor itself.** The CLI `snapshot` reduction object
  reported per-node token sums, which overstate the serialized tree by ~2-3x; it now measures the
  exact pretty-printed payload before and after pruning. In the MCP server, an unchanged snapshot's
  `artifact` handle is now the **same handle** as the reduced response's (`reduction.artifact`) — the
  delta previously stored a second copy of the tree under a different id because the fingerprint was
  taken over the compact serialization while the response was pretty-printed.
- **`extract --mode text` returned the article twice.** `data.text` and `data.blocks` carry the same
  content, so a projected `text` still left the full `blocks` array in the response. When the block
  array alone exceeds the budget it is now dropped and the reduction object says
  `blocks_omitted: true`; `--max-tokens 0` still returns everything.

### Measured (same pages as 0.6.0)

| Call | 0.5.2 | 0.6.1 |
|---|---|---|
| `extract --mode text` (Wikipedia article) | 41,953 | **1,136** (−97%) |
| `snapshot` (rust std index, 900 nodes) | 81,823 | **817** |
| `snapshot`, unchanged page (2nd call, MCP) | 81,823 | **69** |

[0.6.1]: https://github.com/maphim/lightbrowse/compare/v0.6.0...v0.6.1

## [0.6.0] — 2026-09-13

Token-bounded tool output ("context surface"), so a large page can no longer flood an agent's
context — and every reduced answer stays expandable.

### Added

- **`--max-tokens` on the CLI** (`fetch`, `extract --mode text`, `snapshot`): bound the text/tree a
  command prints. Default `1000`, `0` disables. Any shortened response gains a
  `reduction { strategy, original_tokens, reduced_tokens, saved_tokens, id, marker }` object.
- **Model-free observation reducer** (`lightbrowse-core::reduce`): deterministic line/node scoring
  (error/failed/panic/timeout +100, warning/exit +45, per-kind keyword boosts), neighbour-score
  floor around high-signal lines, head+tail fallback, and snapshot-tree pruning with an
  **ancestor/sibling guard** — a kept interactive node never loses the form around it.
- **Raw artifact store** (`lightbrowse-memory::artifacts`, `raw_artifacts` table, schema v2):
  content-addressed `obs_<hash>` payloads, asynchronous writer, read-your-writes, 24h TTL,
  512 MB total cap with LRU eviction, 10 MB per-payload cap (an oversized payload is refused,
  never truncated). The database is the same file as browsing memory (`--memory`).
- **New MCP tools**:
  - `artifact/read(id, offset?, max_chars?)` — the complete pre-reduction payload, byte-identical.
  - `artifact/ask(id, question, limit?)` — ranked passages from a stored artifact, no re-fetch and
    no model call.
  - `artifact/list(limit?, tool?)` — what is still expandable, plus store totals.
  - `tool/inspect(name?)` — the full JSON Schema of one tool, or the compact index of all.
- **Snapshot fingerprint delta**: `snapshot` remembers the last tree fingerprint per URL and
  answers an unchanged page with `{"unchanged": true, "fingerprint": …, "artifact": …}` (~70 tokens)
  instead of the tree. `force: true` always returns the tree.
- **MCP output budget**: `lightbrowse mcp --max-tokens <n>` or `$LIGHTBROWSE_MAX_TOKENS`
  (default `1000`, `0` disables). Shortening only happens when the complete payload has been
  stored, so a bounded answer is always recoverable.

### Changed

- **`tools/list` is compact by default**: names, one-line descriptions, an `args(name, other?)`
  signature and argument names/types instead of full JSON Schema — 4,491 → 2,126 tokens for 41
  tools (**−53%** at session start). Arg names, types and required-ness (encoded as `name?`) are
  preserved; parameter prose and defaults moved behind `tool/inspect`.
  Escape hatch: `--full-tools` or `$LIGHTBROWSE_FULL_TOOLS=1`.
- MCP tool output for read tools is bounded to the budget by default (see Added). Credential and
  control-plane tools (`vault/*`, `login`, `type`, `fill_form`, `cookies`, `runbook/*`,
  `screenshot`, `artifact/read`) are never reduced or stored.
- `SnapshotTree`, `SnapshotNode` and `Bbox` also derive `Deserialize` so a tree can be pruned
  after being parsed back from JSON.

### Fixed

- **Cache hits lost the page structure.** `html_cache` was created but never written or read, so a
  cached page was rebuilt from the text-block index: the second read of any URL returned a page
  with no elements — `snapshot` reported `node_count: 0` and `extract --mode links|forms|meta` came
  back empty. `store_page` now writes the raw HTML and `find_cached` prefers it, with the text-block
  path kept as a documented fallback for rows written before this change.

### Measured

Tokens ≈ output bytes / 4, over MCP stdio with a 1000-token budget:

| Call | Before | After |
|---|---|---|
| `extract` (article text) | 41,947 | **1,062** (−97%) |
| `extract --mode links` (900 links) | 11,508 | **529** (−95%) |
| `snapshot` (900 nodes) | 81,825 | **487** (−99%) |
| `snapshot`, unchanged page (2nd call) | 81,825 | **70** |
| `tools/list` (41 tools) | 4,491 | **2,126** (−53%) |

### Notes / migration

- Defaults changed: MCP tool output is bounded and `tools/list` is compact. `--max-tokens 0` and
  `--full-tools` restore the previous behaviour per process, `$LIGHTBROWSE_MAX_TOKENS=0` and
  `$LIGHTBROWSE_FULL_TOOLS=1` globally.
- Artifacts expire after 24h and survive restarts in the memory database; `artifact/list` shows
  what is still readable.

[0.6.0]: https://github.com/maphim/lightbrowse/compare/v0.5.2...v0.6.0
