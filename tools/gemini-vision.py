#!/usr/bin/env python3
"""
gemini-vision.py — free vision for lightbrowse (no API key).

Sends a screenshot (or any image) to Gemini via the reverse-engineered web API
and returns a text answer. This is the "eyes" of the VisionProvider design
(issue #36 Option A): lightbrowse provides screenshot + bbox map, Gemini
answers "which number is the login button?" → number → click_at.

Cookie is auto-detected from a running Chrome via CDP (port 9222) — zero setup
if Chrome is logged into gemini.google.com. Falls back to config file.

Usage:
  python3 tools/gemini-vision.py --image shot.png --prompt "Which number is the login button? Reply with just the number."
  python3 tools/gemini-vision.py --image shot.png --prompt "Where is the news section?" --json
  python3 tools/gemini-vision.py --image shot.png --prompt "Describe this page layout briefly."

Deps: pip install gemini-webapi
"""

import argparse
import asyncio
import json
import os
import sys
import urllib.request
from pathlib import Path

try:
    from gemini_webapi import GeminiClient
except ImportError:
    print(
        "❌ gemini-webapi missing. Install: pip3 install gemini-webapi --break-system-packages",
        file=sys.stderr,
    )
    sys.exit(1)

CONFIG_FILE = Path(__file__).resolve().parent.parent / ".gemini-vision.json"
MODEL_DEFAULT = "gemini-3-flash"


def _cdp_get_cookies():
    """Auto-extract __Secure-1PSID(+TS) from running Chrome via CDP."""
    try:
        with urllib.request.urlopen("http://127.0.0.1:9222/json", timeout=3) as r:
            targets = json.loads(r.read())
        ws_url = next(
            (t.get("webSocketDebuggerUrl") for t in targets if t.get("webSocketDebuggerUrl")),
            None,
        )
        if not ws_url:
            return None, None
        import websockets

        async def _fetch():
            async with websockets.connect(
                ws_url, max_size=5 * 1024 * 1024, open_timeout=5
            ) as ws:
                await ws.send(json.dumps({"id": 1, "method": "Network.enable"}))
                await asyncio.wait_for(ws.recv(), timeout=5)
                await ws.send(json.dumps({"id": 2, "method": "Network.getCookies"}))
                resp = json.loads(await asyncio.wait_for(ws.recv(), timeout=5))
                cookies = resp.get("result", {}).get("cookies", [])
                spsid = next(
                    (c["value"] for c in cookies if c.get("name") == "__Secure-1PSID"), None
                )
                spsidts = next(
                    (c["value"] for c in cookies if c.get("name") == "__Secure-1PSIDTS"), None
                )
                return spsid, spsidts

        return asyncio.run(_fetch())
    except Exception:
        return None, None


def ensure_cookie() -> tuple[str, str | None]:
    """Cookie from config file or live CDP detection."""
    if CONFIG_FILE.exists():
        try:
            cfg = json.loads(CONFIG_FILE.read_text())
            if cfg.get("secure_1psid"):
                return cfg["secure_1psid"], cfg.get("secure_1psidts")
        except (json.JSONDecodeError, OSError):
            pass
    print("🔍 Auto-detecting cookie from Chrome...", file=sys.stderr)
    spsid, spsidts = _cdp_get_cookies()
    if spsid:
        try:
            CONFIG_FILE.parent.mkdir(exist_ok=True)
            CONFIG_FILE.write_text(
                json.dumps({"secure_1psid": spsid, "secure_1psidts": spsidts})
            )
        except OSError:
            pass
        print("✅ Auto-detected!", file=sys.stderr)
        return spsid, spsidts
    print(
        "❌ No cookie found. Open Chrome (port 9222) logged into gemini.google.com, "
        "or write .gemini-vision.json with {\"secure_1psid\": \"...\"}.",
        file=sys.stderr,
    )
    sys.exit(1)


async def run(image: str, prompt: str, model: str, json_out: bool) -> None:
    if not os.path.exists(image):
        print(f"❌ Image not found: {image}", file=sys.stderr)
        sys.exit(1)
    spsid, spsidts = ensure_cookie()
    try:
        client = GeminiClient(spsid, spsidts or None)
        await client.init(timeout=120, auto_refresh=False, verbose=False)
        print(f"📷 {image} → {model}", file=sys.stderr)
        # Chat flow (start_chat + send_message) — more robust than the raw
        # generate_content path against the reverse-engineered web API.
        chat = client.start_chat(model=model)
        resp = await chat.send_message(prompt, files=[image], temporary=True)
        text = resp.text or ""
    except Exception as e:
        print(f"❌ Gemini error: {e}", file=sys.stderr)
        sys.exit(1)
    finally:
        try:
            await client.close()
        except Exception:
            pass

    if json_out:
        print(json.dumps({"text": text}, ensure_ascii=False))
    else:
        print(text)


def main():
    ap = argparse.ArgumentParser(description="Free Gemini vision for lightbrowse")
    ap.add_argument("--image", required=True, help="path to screenshot/image")
    ap.add_argument("--prompt", required=True, help="question about the image")
    ap.add_argument("--model", default=MODEL_DEFAULT, help=f"gemini model (default {MODEL_DEFAULT})")
    ap.add_argument("--json", action="store_true", help="emit JSON output")
    args = ap.parse_args()
    asyncio.run(run(args.image, args.prompt, args.model, args.json))


if __name__ == "__main__":
    main()
