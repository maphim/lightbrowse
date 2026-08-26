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
# CLI
lightbrowse fetch    https://example.com
lightbrowse extract  https://example.com --mode links
lightbrowse snapshot https://example.com
lightbrowse search   "rust async runtime"

# HTTP API
lightbrowse serve --port 8787
curl 'http://127.0.0.1:8787/v1/extract?url=https://example.com&mode=meta'

# MCP server (stdio) — wire this into your agent host
lightbrowse mcp
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
| `navigate(url)` | fetch + title/status/word-count + text preview |
| `extract(url, mode)` | structured `text` \| `links` \| `forms` \| `meta` \| `headings` |
| `snapshot(url, max_nodes?)` | accessibility tree with stable uids |
| `search(query, max_results?)` | DuckDuckGo results (title/url/snippet) |

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
├── lightbrowse-core/   engine-agnostic types, extractors, snapshot, session
├── lightbrowse-fetch/  default backend: reqwest (gzip/brotli, cookies)
├── lightbrowse-mcp/    MCP stdio server (JSON-RPC 2.0, zero extra deps)
├── lightbrowse-http/   axum REST API
└── lightbrowse-cli/    the `lightbrowse` binary
```

## Build

```bash
cargo build --release            # one binary: target/release/lightbrowse
cargo test --workspace           # unit tests
cargo clippy --workspace -- -D warnings
```

Requires Rust 1.75+. Binary is stripped + `opt-level=s` (size-oriented).

## Roadmap

- [ ] **cdp backend** — drive real Chrome via DevTools Protocol for JS-heavy sites
- [ ] **webview backend** — embedded GUI + screenshots (wry)
- [ ] interaction actions: `click(uid)`, `type(uid, text)`, `submit(form)`
- [ ] session persistence to disk (survive restarts)
- [ ] JS execution (boa_engine) for light scripting
- [ ] proxy / socks support, stealth fingerprinting options

## License

MIT © maphim
