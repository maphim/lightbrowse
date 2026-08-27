# lightbrowse 🪶

**A featherweight, AI-native browser in Rust.**

lightbrowse turns the web into **structured data your LLM agent can use** —
no browser engine, no GUI, no Node.js, no API key. One small binary that
navigates, extracts, snapshots and searches, exposed through a CLI, an
HTTP/REST API, and an **MCP server** (Model Context Protocol) so any AI host
(Claude, pi, Cursor, …) can drive it as a tool.

```
┌─────────────────────────────────────────────────────────────┐
│  any LLM agent                                              │
│    │  MCP (stdio)          │  HTTP/REST                     │
│    ▼                       ▼                                │
│  ┌─────────────┐   ┌──────────────┐   ┌──────────────────┐  │
│  │  mcp server │   │  http api    │   │  cli             │  │
│  └──────┬──────┘   └──────┬───────┘   └────────┬─────────┘  │
│         └─────────────────┼────────────────────┘            │
│                           ▼                                │
│              ┌───────────────────────────┐                 │
│              │  lightbrowse-core         │                 │
│              │  session · cookies ·      │                 │
│              │  extractors · snapshot    │                 │
│              └───────────┬───────────────┘                 │
│                          ▼                                │
│              ┌───────────────────────────┐                 │
│              │  backends (pluggable)     │                 │
│              │  ▸ fetch (pure Rust, ✓)   │                 │
│              │  ▸ cdp (Chrome, planned)  │                 │
│              │  ▸ webview (GUI, planned) │                 │
│              └───────────────────────────┘                 │
└─────────────────────────────────────────────────────────────┘
```

## Why lightbrowse?

| Problem | lightbrowse |
|---|---|
| Playwright/Selenium are heavy & language-locked | one ~10 MB Rust binary, zero browser engine |
| Raw HTML is too noisy for LLMs | readability text extraction + accessibility snapshots |
| Cookies/sessions lost between tool calls | shared session with persistent cookie jar |
| Agents need structure, not pixels | `links`, `forms`, `meta`, `headings`, stable `uid` snapshot |
| API-key-gated search | built-in DuckDuckGo search (no key) |

## Features

- 🪶 **Featherweight** — pure-Rust fetch backend; no Chromium, no WebKit, no GUI
- 🧠 **AI-native output** — readability text, link/form/meta/heading extractors,
  accessibility-style tree with stable `uid`s an agent can reference
- 🍪 **Real browsing state** — shared cookie jar + history per session
  (log in once, keep using it)
- 🔌 **Three interfaces, one core** — CLI, HTTP/REST, MCP (stdio)
- 🔍 **Built-in search** — DuckDuckGo lite, zero API key
- 🧩 **Pluggable backends** — trait-based; CDP (real Chrome) and webview
  (GUI + screenshots) are on the roadmap

## Quick start

```bash
# CLI — featherweight fetch engine by default
lightbrowse fetch    https://example.com
lightbrowse extract  https://example.com --mode links
lightbrowse snapshot https://example.com
lightbrowse search   "rust async runtime"

# CLI — headless Chromium for JS-rendered sites (lazy spawn, auto-suspend)
lightbrowse fetch    https://spa.example --engine cdp
lightbrowse fetch    https://spa.example --engine auto   # auto-fallback

# HTTP API
lightbrowse serve --port 8787 --engine auto --idle-timeout 30
curl 'http://127.0.0.1:8787/v1/extract?url=https://example.com&mode=meta'

# MCP server (stdio) — wire this into your agent host
lightbrowse mcp --engine auto
```

## MCP integration

Any MCP-capable host can use lightbrowse as a tool server. Example (Claude
Desktop / pi config):

```json
{
  "mcpServers": {
    "lightbrowse": {
      "command": "/usr/local/bin/lightbrowse",
      "args": ["mcp"]
    }
  }
}
```

Available tools:

| Tool | Description |
|---|---|
| `navigate(url, engine?)` | title/status/word-count + text preview |
| `extract(url, mode, engine?)` | structured `text` \| `links` \| `forms` \| `meta` \| `headings` |
| `snapshot(url, max_nodes?, engine?)` | accessibility tree with stable uids + CSS selectors |
| `ask(url, question, engine?)` | intent-aware: fetch/cache + scored relevant blocks |
| `click(selector)` | click an element on the active CDP tab (from snapshot) |
| `type(selector, text)` | type into an input (React-compatible events) |
| `submit(selector)` | submit the element's form |
| `page/current` | read the active CDP tab after actions |
| `memory/search(query)` | BM25 search over everything read |
| `memory/recent(limit?)` | recently read pages |
| `search(query, max_results?)` | DuckDuckGo results (title/url/snippet) |

**Agent interaction loop:** `navigate(url, engine="cdp")` → `snapshot()` → act
(`click`/`type`/`submit` with a snapshot `selector`) → `page/current` to see
the result. `engine="cdp"` keeps the tab open; cached pages are read-only.

`engine` is `auto` by default: fetch first, headless Chromium fallback for
JS-rendered pages.`

### HTTP API

| Endpoint | Query params |
|---|---|
| `GET /health` | — |
| `GET /v1/page` | `url`, `engine` |
| `GET /v1/extract` | `url`, `mode`, `engine` |
| `GET /v1/snapshot` | `url`, `max_nodes`, `engine` |
| `GET /v1/search` | `q`, `max_results` |

## HTTP API

| Endpoint | Query params |
|---|---|
| `GET /health` | — |
| `GET /v1/page` | `url` |
| `GET /v1/extract` | `url`, `mode` |
| `GET /v1/snapshot` | `url`, `max_nodes` |
| `GET /v1/search` | `q`, `max_results` |

## Architecture

Cargo workspace:

```
crates/
├── lightbrowse-core/   engine-agnostic types, extractors, snapshot, session, config
├── lightbrowse-fetch/  tier-1 backend: pure-Rust reqwest (gzip/brotli, cookies)
├── lightbrowse-cdp/    tier-2 backend: headless Chromium via hand-rolled CDP client
├── lightbrowse-mcp/    MCP stdio server (JSON-RPC 2.0, zero extra deps)
├── lightbrowse-http/   axum REST API
└── lightbrowse-cli/    the `lightbrowse` binary
```

## Logging in (Gmail, brokerage accounts, ...)

lightbrowse is built to get *past* login walls, not to fight CAPTCHAs:

1. **Persistent profile** — `--profile <dir>` (or `LIGHTBROWSE_PROFILE`) keeps
   cookies + localStorage across restarts. Log in once, the session survives:
   ```bash
   lightbrowse serve --profile ~/.config/lightbrowse/gmail
   # AI logs in via CDP actions → idle → Chromium closes gracefully
   # (cookies flushed) → next run is still logged in
   ```
   Chromium is closed with CDP `Browser.close` (not SIGKILL) so cookies are
   flushed to the profile.
2. **Stealth by default** — clean Chrome UA (no `HeadlessChrome`, no
   `lightbrowse/` marker), `navigator.webdriver` neutered, `window.chrome` /
   `plugins` / `languages` spoofed. Verified against bot.sannysoft.com.
3. **Real input events** — clicks are dispatched as mouse events at the
   element's coordinates (moved → pressed → released); typing uses CDP
   `Input.insertText` (real keyboard events, React-compatible); keys via
   `Input.dispatchKeyEvent`. Anti-bot heuristics see a human, not `el.click()`.

**Honest limits:** Google/Cloudflare may still challenge headless Chromium
with CAPTCHA — that's an arms race. For 2FA (TOTP/SMS) the agent should stop
and ask the human; lightbrowse won't (and shouldn't) bypass that. Brokerages
in VN are generally much lighter than Google.

## Runbooks — stop re-discovering selectors

The first time an agent does something fiddly (logging in, filling a form),
it pokes around: snapshots, tries selectors, retries. That trial-and-error is
**recorded automatically** — every successful `click`/`type`/`press` lands in
the session trail — and can be saved as a named **runbook**:

```
# first time: agent fumbles through the login
navigate("https://accounts.example/login", engine="cdp")
type("#email", "me@example.com")   # recorded
press("Tab")                        # recorded
type("#password", "s3cret")        # recorded
press("Enter")                      # recorded
runbook/save {"name": "login-example"}

# second time: no fumbling
runbook/run {"name": "login-example"}
# -> replays all steps, tries selector fallbacks (id/name/placeholder),
#    returns the final page state so you can confirm "Logged in"
```

| Tool | What it does |
|---|---|
| `runbook/save {name}` | save the session trail as a runbook (auto-recorded actions) |
| `runbook/run {name, variables?}` | replay; `{{EMAIL}}`-style placeholders get substituted |
| `runbook/get {name}` | fetch steps — hand them to the agent as a plan |
| `runbook/list` | all saved runbooks |
| `trail/clear` | start a fresh recording |

Steps carry **selector fallbacks** (`#id`, `[name=...]`, `[placeholder=...]`,
`[aria-label=...]`) so replays survive small DOM changes. Runbooks live in
the same SQLite store as browsing memory.

## Browsing memory (SQLite + FTS5)

Everything the browser reads is cached and indexed at
`~/.cache/lightbrowse/memory.db` (or `--memory <path>`):

- **URL cache with TTL** — repeat fetches of the same page skip the network
- **BM25 search** — `memory/search "what did we read about X"` finds blocks
  across pages (with a substring fallback for stopword-heavy queries)
- **Recent history** — `memory/recent`
- **Intent-aware ask** — `ask <url> "question"` returns only the relevant
  blocks + scores instead of the whole page (token-efficient for agents)

No vector DB and no knowledge graph inside the browser by design: semantic
retrieval is delegated to the host's memory system (e.g. MemPalace via MCP).

## Engines & RAM strategy

lightbrowse is a **two-tier browser**, because "good web support" and "low
RAM" are opposing goals — so you only pay for what you need:

| Tier | Engine | RAM | JS | When |
|---|---|---|---|---|
| 1 | `fetch` (pure Rust) | ~5 MB | ❌ | default for readable sites |
| 2 | `cdp` (headless Chromium) | ~200 MB | ✅ | pages that render with JS |

- **Headless-first**: the UI is off by default (`ui: false`); a window is an
  opt-in feature for humans who want to watch.
- **Lazy spawn**: Chromium is only launched when a page actually needs it —
  `--engine auto` tries fetch first and falls back to Chromium for
  JS-rendered pages (empty content + `<script>` heuristic).
- **Idle suspension**: a tab that sits unused for `idle_timeout_secs`
  (default 60 s) gets its network dropped — the Chromium process is killed
  and its RAM released. A watcher polls every 5 s.
- **Memory budget**: `memory_budget_mb` (default 1024) sizes the JS heap
  (`--max-old-space-size`) and caps concurrent tabs.
- **Lazy JS wait**: `js_wait_ms` (default 800) extra grace after the load
  event for frameworks that render late.

### Config

| CLI flag / env | Default | Meaning |
|---|---|---|
| `--engine auto\|fetch\|cdp` | `auto` | engine selection (CLI/MCP/HTTP) |
| `--idle-timeout <secs>` | `60` | suspend Chromium after idle (serve) |
| `LIGHTBROWSE_MEMORY_MB` | `1024` | RAM budget |
| `LIGHTBROWSE_IDLE_TIMEOUT` | `60` | idle timeout for one-shot modes |
| `LIGHTBROWSE_UI` | unset | set to enable GUI (roadmap) |
| `CHROME_PATH` | auto-detect | Chrome/Chromium binary |

## Build

```bash
cargo build --release            # one binary: target/release/lightbrowse
cargo test --workspace           # unit tests
cargo clippy --workspace -- -D warnings
```

Requires Rust 1.75+. Binary is stripped + `opt-level=s` (size-oriented).

## Roadmap

- [x] **fetch backend** — pure Rust, ~5 MB RAM
- [x] **cdp backend** — hand-rolled CDP client, lazy Chromium spawn, idle suspension
- [x] **auto engine** — fetch first, Chromium fallback for JS-rendered pages
- [ ] interaction actions: `click(uid)`, `type(uid, text)`, `submit(form)` (CDP)
- [ ] screenshots (CDP `Page.captureScreenshot`)
- [ ] **webview backend** — optional GUI window (wry), off by default
- [ ] session persistence to disk (survive restarts)
- [ ] multi-tab resource manager with per-tab suspend
- [ ] proxy / socks support, stealth fingerprinting options

## License

MIT © maphim
