# lightbrowse Roadmap

> A featherweight, AI-native browser in Rust. Headless-first, RAM-budgeted,
> built for agents — humans are optional spectators.

**Status legend:** `[x]` done · `[~]` in progress · `[ ]` planned

## Core engine

- [x] **Tier-1 fetch backend** — pure Rust (reqwest + rustls), ~5 MB RAM
- [x] **Tier-2 CDP backend** — hand-rolled DevTools client, lazy Chromium spawn
- [x] **Auto engine** — fetch first, headless Chromium fallback for JS pages
- [x] **Idle suspension** — Chromium killed after idle timeout, RAM released
- [x] **Low-memory mode** — budget < 350 MB sheds processes/cache
- [x] **Real-RAM reporting** — `/health` exposes live `self_ram_mb`, `cdp_ram_mb`, peak, navs
- [x] **Self-healing** — dead-Chromium detection, kill + respawn + retry once
- [x] **Orphan cleanup** — Chromium killed on shutdown (no headless orphans)
- [x] **Token-optimized outputs** — compact snapshot, clipped ask hits (benchmarked)
- [ ] **Resource manager** — per-tab budgets, eviction, global memory governor
- [x] **Proxy / SOCKS** support — http/https/socks5/socks5h, both engines,
      runtime-switchable (`--proxy`, `LIGHTBROWSE_PROXY`, `PUT /v1/proxy`, MCP `proxy/set`)
- [x] **Resource manager** — per-session tabs with LRU eviction at
      `--max-tabs`, RAM governor evicts idle tabs over budget,
      `tabs/list` + `tab/close` (HTTP + MCP)
- [x] **Session isolation** — named `?session=<id>` contexts (own cookies +
      own tab); actions target the right tab
- [x] **Iframe support** — snapshot/click/type/submit see and drive fields
      inside iframes (Microsoft fpt.live.com login, etc.) via CDP frame
      contexts + viewport offset math
- [x] **OOPIF support** — cross-origin iframes reached via `Target.getTargets`
      (Chrome doesn't list them in `Page.getFrameTree`)
- [x] **Tab-level recovery** — read-only tools self-heal after a renderer
      crash (`Connection reset without closing handshake` / 20s hangs)
- [x] **Renderer crash logging** — `Target.targetCrashed` listener attributes
      resets to real crashes instead of guessed network issues
- [x] **SSO auth sharing** — all CDP sessions share the persistent Chromium
      profile: log into Microsoft SSO once, Outlook/Teams/SharePoint are
      auto-authenticated everywhere
- [ ] **Stealth options** — fingerprinting toggles for bot-detected sites

## Reading & extraction (what agents need most)

- [x] Readability text, links, forms, meta, headings extractors
- [x] Accessibility snapshot with stable `uid`s
- [x] CSS `selector` on snapshot nodes (for actions)
- [x] **Intent-aware `ask`** — pass a question, get relevant blocks + score
- [x] **Token & cost section** — benchmarked savings vs naive reads (see README)
- [ ] **Page summarization** — optional LLM-less extractive summary
- [ ] **Diff mode** — compare two versions of a page
- [ ] **Multi-page research** — batch N URLs, aggregated answer

## Browsing memory (why no vector DB — yet)

> Strategy: **session memory in-process/SQLite + FTS5; semantic retrieval is
> delegated to the host's memory system (e.g. MemPalace via MCP).** No vector
> DB inside the browser until a research corpus outgrows FTS (hundreds of
> pages), and even then it's an optional `lightbrowse-memory` feature.

- [x] **SQLite store** — pages, blocks, cache, FTS5 index
- [x] **URL cache** with TTL — skip re-fetching what we already have
- [x] **Search what we read** — `memory/search` over page blocks (BM25)
- [x] **Recent history** — `memory/recent`
- [x] **Session persistence** (file-backed store)
- [x] **Entity extraction** — emit entities/facts to host memory (MCP hook) — survive restarts (file-backed store)


## Actions (make the browser *do* things)

- [x] **click(selector)** via CDP — real mouse events at element coordinates
- [x] **type(selector, text)** — CDP `Input.insertText` (real keyboard events)
- [x] **press(key)** — `Input.dispatchKeyEvent` (Enter/Tab/Backspace/...)
- [x] **submit(selector)** — `form.requestSubmit()`
- [x] **persistent profile** — logins survive restarts (graceful Browser.close)
- [x] **stealth** — clean UA, webdriver/plugins/chrome spoof (bot.sannysoft ✓)
- [x] **runbooks** — auto-record action trail → save/run/list/get recipes;
      selector fallbacks + {{VAR}} substitution; final-state verification
- [x] **credential vault** — encrypted (AES-256-GCM) password storage;
      `vault/set|list|get|delete` (CLI + MCP + HTTP); runbook replay can
      resolve `vault:<name>.<field>` server-side so secrets never enter
      the LLM context
- [x] **screenshot** — `Page.captureScreenshot` (CDP)
- [x] **session cookies across actions** — persist logged-in state
- [x] **downloads** — `Browser.setDownloadBehavior` + `download`/`downloads`
      tools (MCP + HTTP), configurable dir (`--download-dir`), multiple
      downloads per session, post-download rename to requested filename
- [x] **network capture** — `network/capture start|stop|flush|log` records
      requests/responses/failures for SPA API discovery + auth analysis
- [x] **attach to running Chrome** — `--cdp-url` reuses a logged-in browser
      (never closes/kills it); `--preload` injects JS before app scripts
- [x] **navigate hardening** — errors only surface when the final retry
      fails; `Page.getFrameTree` retried with backoff; SPA settle wait
- [ ] **waits** — wait-for-selector / network-idle helpers

## Interfaces

- [x] CLI (`fetch`, `extract`, `snapshot`, `search`, `ask`, `memory-*`, `serve`, `mcp`)
- [x] MCP server — navigate/extract/snapshot/search/ask/research/memory/click/type/submit/press/screenshot/evaluate/runbook
- [x] HTTP API — `/v1/{page,extract,snapshot,search,ask,memory,click,type,submit,current,cookies,download,downloads,network/log,network/capture}` + `/docs` + `/openapi.json`
- [ ] WebSocket streaming for long-running research tasks
- [ ] Optional GUI (wry) — off by default, for humans who want to watch

## Non-goals (deliberately)

- No bundled browser engine (Servo/WebKit) — Chromium via CDP is the JS tier
- No vector DB / knowledge graph inside the browser — delegate to host memory
- No plugin system in v1 — the MCP surface *is* the extension point
