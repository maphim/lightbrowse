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
- [x] **Stealth options** — fingerprinting toggles for bot-detected sites
      (`--no-stealth`; built-in injection: webdriver/UA/chrome/plugins/
      languages/hardwareConcurrency/deviceMemory). Note: hard CF routes
      (e.g. voz `/whats-new/`) may still block headless — see Stealth v2 below.

## Vision-grounded perception (the agent sees like a human) 🪶👁️

> **Paradigm:** screenshot + Set-of-Mark (SoM) numbered overlay → vision LLM
> picks numbers like a human pointing → `click_at(x,y)` acts. The DOM is the
> fallback, not the primary sense.

- [x] **bbox per snapshot node** — `x/y/w/h` viewport rects, one JS
      `getBoundingClientRect` pass (CDP only; fetch omits) — also gives
      text-only LLMs spatial reasoning
- [x] **`visual_snapshot`** — screenshot + numbered SoM frames over
      interactive elements + `number→{uid,text,bbox}` map
      (MCP / HTTP `/v1/visual_snapshot` / CLI `visual-snapshot`; embedded
      5×7 bitmap font, no font files; `--settle-ms` waits out bot challenges)
- [x] **`click_at(x,y)`** — coordinate click via `Input.dispatchMouseEvent`
      (MCP / HTTP `/v1/click_at` / CLI `click-at`)
- [x] **Gemini Web vision sidecar** (`tools/gemini-vision.py`) — FREE
      reverse-engineered Gemini Web API, no API key, CDP cookie auto-detect.
      Verified live: reads SoM overlay, identifies elements/threads.
- [ ] **`visual_snapshot mode=gemini`** — Rust calls the sidecar server-side
      (python subprocess), returns the vision answer directly — vision becomes
      a lightbrowse feature, no host vision required
- [ ] **VisionProvider trait** — pluggable `locate(image, prompt, candidates)
      → {soom_id|uid|coords}`; providers: gemini-web (free) / anthropic /
      openai / omni-parser (local)
- [ ] **`click <n>` SoM convenience** — resolve number → bbox center in one
      call (MCP/HTTP)
- [ ] **`hover_at(x,y)`** — for hover-dependent menus
- [ ] **post-click verification** — `elementFromPoint` check after click
      (clicked the right thing? page changed?)
- [ ] **Stealth v2 (undetected)** — CF-hard routes (voz `/whats-new/`,
      cloudflare Turnstile) need a fuller human fingerprint (canvas noise,
      CDP-detection patches, cf_clearance flow)

## Reading & extraction (what agents need most)

- [x] Readability text, links, forms, meta, headings extractors
- [x] Accessibility snapshot with stable `uid`s
- [x] CSS `selector` on snapshot nodes (for actions)
- [x] **Intent-aware `ask`** — pass a question, get relevant blocks + score
- [x] **Token & cost section** — benchmarked savings vs naive reads (see README)
- [x] **Page summarization** — LLM-less extractive summary
      (`lightbrowse summarize <url>`, TF×position scoring, timestamp-noise
      filter; no model, no API)
- [x] **Diff mode** — `lightbrowse diff <urlA> <urlB>`: line-level LCS,
      context compaction, change stats (CLI); MCP/HTTP expose planned
- [ ] **Multi-page research** — batch N URLs, aggregated answer
      (research tool exists in MCP — CLI batch mode planned)

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
- [x] **click_at(x,y)** — coordinate click (SoM/vision “human pointing”)
- [x] **login(username, password)** — ONE-CALL login: direct JS detection of
      username+password fields (depth-independent), fill both, Enter to submit
      (MCP `login` / HTTP `/v1/login` / CLI `login <url> <user> <pass>`;
      secrets can reference the vault as `vault:<name>.field`)
- [x] **fill_form(values, auto, submit)** — ONE-CALL form/survey filler like a
      human: enumerates every editable field (inputs/selects/textareas/
      checkboxes/radios with labels), matches caller values by label/name/id/
      placeholder, auto-generates test data for the rest (VN-aware: tên, sđt,
      tuổi, quốc gia…), fills via real input events, optional submit button
      detection (MCP `fill_form` / HTTP `POST /v1/form/fill` / CLI
      `fill-form <url> --values '{...}'`). Live-verified on a 11-field
      registration form — all filled in one call
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
- [ ] **WebSocket streaming for long-running research tasks**
- [ ] **Optional GUI (wry)** — off by default, for humans who want to watch

## Non-goals (deliberately)

- No bundled browser engine (Servo/WebKit) — Chromium via CDP is the JS tier
- No vector DB / knowledge graph inside the browser — delegate to host memory
- No plugin system in v1 — the MCP surface *is* the extension point
