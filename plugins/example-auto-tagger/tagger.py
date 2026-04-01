#!/usr/bin/env python3
"""
Example auto-tagger plugin for LightView.

Protocol:
  - Reads a JSON object from stdin with fields:
      action:     str — the action to perform (e.g. "tag")
      media_path: str — absolute path to the media file
  - Writes a JSON object to stdout with fields:
      tags: list[str]   — tags to assign under this plugin's namespace
      meta: dict | null — optional metadata to store alongside tags

The plugin never reads or writes companion files — the host app handles
all file I/O, batching, and index updates.
"""

import json
import os
import sys


def main():
    raw = sys.stdin.read()
    request = json.loads(raw)

    action = request.get("action", "tag")
    media_path = request.get("media_path", "")

    if action == "tag":
        # Always add the "example" tag.
        # A real plugin would analyse the image here (EXIF, ML model, etc.).
        tags = ["example"]

        # Derive a simple filename-based tag as a bonus demonstration.
        basename = os.path.splitext(os.path.basename(media_path))[0].lower()
        tags.append(f"file:{basename}")

        result = {
            "tags": tags,
            "meta": {
                "source": "example-auto-tagger",
                "note": "This tag was added by the example plugin.",
            },
        }
    else:
        # Unknown action — return empty output.
        result = {"tags": [], "meta": None}

    json.dump(result, sys.stdout)


if __name__ == "__main__":
    main()
