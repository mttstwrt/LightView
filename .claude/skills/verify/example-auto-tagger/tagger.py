#!/usr/bin/env python3
"""
The verification fixture: the smallest plugin that conforms to the protocol in
the README's "Writing a plugin" section. drive.sh and grid.mjs install it to
exercise the host's plugin path; they read its name, its `example` tag and its
manifest version, so change those together with the scripts.

Streaming protocol (newline-delimited JSON):
  Stdin:  {"action": "tag", "path": "/abs/path/img.jpg"}\n  ...
  Stdout: {"path": "...", "tags": [...], "meta": {...}}\n   ...

One result per request, written as each line arrives. Never buffer until EOF:
the host releases an in-flight slot only when a result comes back.
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
