#!/usr/bin/env python3
"""
Example auto-tagger plugin for LightView.

Streaming protocol (newline-delimited JSON):
  Stdin:  {"action": "tag", "path": "/abs/path/img.jpg"}\n  ...
  Stdout: {"path": "...", "tags": [...], "meta": {...}}\n   ...

The plugin reads stdin to EOF and writes one result per request. The host
handles all companion-file I/O, batching, and index updates.
"""

import json
import os
import sys


def handle(request):
    path = request.get("path", "")
    action = request.get("action", "tag")

    if action != "tag":
        return {"path": path, "tags": [], "error": f"Unknown action: {action}"}

    tags = ["example"]
    basename = os.path.splitext(os.path.basename(path))[0].lower()
    tags.append(f"file:{basename}")

    return {
        "path": path,
        "tags": tags,
        "meta": {
            "source": "example-auto-tagger",
            "note": "This tag was added by the example plugin.",
        },
    }


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError as e:
            sys.stdout.write(json.dumps({"path": "", "tags": [], "error": f"Invalid JSON: {e}"}) + "\n")
            sys.stdout.flush()
            continue
        sys.stdout.write(json.dumps(handle(request)) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
