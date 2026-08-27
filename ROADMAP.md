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
- [x] **Real-RAM reporting** — `/health` exposes live `cdp_ram_mb`
- [ ] **Resource manager** — per-tab budgets, eviction, global memory governor
- [ ] **Proxy / SOCKS** support
- [ ] **Stealth options** — fingerprinting toggles for bot-detected sites

## Reading & extraction (what agents need most)

- [x] Readability text, links, forms, meta, headings extractors
- [x] Accessibility snapshot with stable `uid`s
- [x] CSS `selector` on snapshot nodes (for actions)
- [x] **Intent-aware `ask`** — pass a question, get relevant blocks + score
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

- [x] **click(selector)** via CDP — `document.querySelector().click()`
- [x] **type(selector, text)** — native value setter + input/change events
- [x] **submit(selector)** — `form.requestSubmit()`
- [ ] **screenshot** — `Page.captureScreenshot` (CDP)
- [ ] **session cookies across actions** — persist logged-in state
- [ ] **waits** — wait-for-selector / network-idle helpers

## Interfaces

- [x] CLI (`fetch`, `extract`, `snapshot`, `search`, `ask`, `memory-*`, `serve`, `mcp`)
- [x] MCP server — navigate/extract/snapshot/search/ask/memory/click/type/submit
- [x] HTTP API — `/v1/{page,extract,snapshot,search,ask,memory,click,type,submit,current}`
- [ ] WebSocket streaming for long-running research tasks
- [ ] Optional GUI (wry) — off by default, for humans who want to watch

## Non-goals (deliberately)

- No bundled browser engine (Servo/WebKit) — Chromium via CDP is the JS tier
- No vector DB / knowledge graph inside the browser — delegate to host memory
- No plugin system in v1 — the MCP surface *is* the extension point
