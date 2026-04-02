#!/usr/bin/env python3
"""
WD Eva02 Large Tagger v3 plugin for LightView.

Uses the SmilingWolf/wd-eva02-large-tagger-v3 ONNX model to predict
danbooru-style tags for images. The model and label files are downloaded
from HuggingFace on first run and cached locally.

Dependencies (install once):
    pip install huggingface_hub numpy onnxruntime pandas pillow

Protocol:
  - Reads JSON from stdin: {"action": "tag", "media_path": "/path/to/image"}
  - Writes JSON to stdout: {"tags": [...], "meta": {...}}
"""

import ctypes
import glob as _glob
import json
import shutil
import sys
import os

# Pre-load CUDA 12 shared libraries from pip-installed nvidia packages so
# that onnxruntime's C++ dlopen() calls can find them.  Simply setting
# LD_LIBRARY_PATH after process start is not sufficient — we need the
# libraries in the linker's namespace before onnxruntime tries to load
# its CUDA provider.
_site = os.path.join(os.path.dirname(os.path.abspath(__file__)), ".venv", "lib")
_nvidia_lib_dirs = sorted(_glob.glob(
    os.path.join(_site, "python*", "site-packages", "nvidia", "*", "lib")
))
# Load order: runtime first, then libs that depend on it.
_CUDA_PRELOAD = [
    "libcudart.so*",
    "libnvrtc.so*",
    "libnvJitLink.so*",
    "libcublas.so*",
    "libcublasLt.so*",
    "libcufft.so*",
    "libcurand.so*",
    "libcudnn.so*",
]
for _pattern in _CUDA_PRELOAD:
    for _d in _nvidia_lib_dirs:
        _matches = sorted(_glob.glob(os.path.join(_d, _pattern)))
        for _path in _matches:
            # Skip static archives and symlinks-to-self; load the real .so
            if _path.endswith(".a"):
                continue
            try:
                ctypes.CDLL(_path, mode=ctypes.RTLD_GLOBAL)
            except OSError:
                pass

import huggingface_hub
import numpy as np
import onnxruntime as rt
import pandas as pd
from PIL import Image

MODEL_REPO = "SmilingWolf/wd-eva02-large-tagger-v3"
MODEL_FILENAME = "model.onnx"
LABEL_FILENAME = "selected_tags.csv"

GENERAL_THRESHOLD = 0.35
CHARACTER_THRESHOLD = 0.85

# Kaomoji tags that should not have underscores replaced with spaces
KAOMOJIS = {
    "0_0", "(o)_(o)", "+_+", "+_-", "._.", "<o>_<o>", "<|>_<|>",
    "=_=", ">_<", "3_3", "6_9", ">_o", "@_@", "^_^", "o_o",
    "u_u", "x_x", "|_|", "||_||",
}


PLUGIN_DIR = os.path.dirname(os.path.abspath(__file__))


def download_model():
    csv_path = os.path.join(PLUGIN_DIR, LABEL_FILENAME)
    model_path = os.path.join(PLUGIN_DIR, MODEL_FILENAME)

    if not os.path.isfile(csv_path):
        downloaded = huggingface_hub.hf_hub_download(MODEL_REPO, LABEL_FILENAME)
        shutil.copy2(downloaded, csv_path)

    if not os.path.isfile(model_path):
        downloaded = huggingface_hub.hf_hub_download(MODEL_REPO, MODEL_FILENAME)
        shutil.copy2(downloaded, model_path)

    return csv_path, model_path


def load_labels(csv_path):
    df = pd.read_csv(csv_path)
    names = df["name"].map(
        lambda x: x.replace("_", " ") if x not in KAOMOJIS else x
    ).tolist()
    rating_idxs = list(np.where(df["category"] == 9)[0])
    general_idxs = list(np.where(df["category"] == 0)[0])
    character_idxs = list(np.where(df["category"] == 4)[0])
    return names, rating_idxs, general_idxs, character_idxs


def prepare_image(image_path, target_size):
    image = Image.open(image_path).convert("RGBA")
    canvas = Image.new("RGBA", image.size, (255, 255, 255))
    canvas.alpha_composite(image)
    image = canvas.convert("RGB")

    # Pad to square
    max_dim = max(image.size)
    pad_left = (max_dim - image.size[0]) // 2
    pad_top = (max_dim - image.size[1]) // 2
    padded = Image.new("RGB", (max_dim, max_dim), (255, 255, 255))
    padded.paste(image, (pad_left, pad_top))

    if max_dim != target_size:
        padded = padded.resize((target_size, target_size), Image.BICUBIC)

    arr = np.asarray(padded, dtype=np.float32)
    # RGB -> BGR (model expects BGR)
    arr = arr[:, :, ::-1]
    return np.expand_dims(arr, axis=0)


class Tagger:
    """Holds a loaded ONNX model and label data for reuse across requests."""

    def __init__(self):
        csv_path, model_path = download_model()
        self.tag_names, self.rating_idxs, self.general_idxs, self.character_idxs = load_labels(csv_path)

        providers = []
        available = rt.get_available_providers()
        if "CUDAExecutionProvider" in available:
            providers.append(("CUDAExecutionProvider", {
                "device_id": int(os.environ.get("CUDA_DEVICE", "0")),
                "arena_extend_strategy": "kSameAsRequested",
                "cudnn_conv_algo_search": "DEFAULT",
            }))
        providers.append(("CPUExecutionProvider", {}))

        sess_options = rt.SessionOptions()
        # Limit to single inference thread to avoid VRAM contention with the
        # UI's rendering process (Phase 2.3 concurrency gating).
        max_threads = int(os.environ.get("ONNX_THREADS", "1"))
        sess_options.intra_op_num_threads = max_threads
        sess_options.inter_op_num_threads = max_threads
        sess_options.execution_mode = rt.ExecutionMode.ORT_SEQUENTIAL

        # Redirect stdout to /dev/null during ONNX session creation —
        # onnxruntime's C++ layer can print provider info directly to fd 1,
        # which corrupts the JSON protocol on stdout.
        saved_fd = os.dup(1)
        devnull = os.open(os.devnull, os.O_WRONLY)
        os.dup2(devnull, 1)
        os.close(devnull)
        try:
            self.model = rt.InferenceSession(model_path, sess_options, providers=providers)
        finally:
            os.dup2(saved_fd, 1)
            os.close(saved_fd)

        active_providers = self.model.get_providers()
        self.using_gpu = "CUDAExecutionProvider" in active_providers
        sys.stderr.write(f"wd-tagger: active providers = {active_providers}\n")
        sys.stderr.flush()

        _, height, width, _ = self.model.get_inputs()[0].shape
        self.target_size = height
        self.input_name = self.model.get_inputs()[0].name
        self.label_name = self.model.get_outputs()[0].name

    def predict(self, image_path):
        image_array = prepare_image(image_path, self.target_size)
        preds = self.model.run([self.label_name], {self.input_name: image_array})[0]

        labels = list(zip(self.tag_names, preds[0].astype(float)))

        rating_labels = {labels[i][0]: labels[i][1] for i in self.rating_idxs}
        top_rating = max(rating_labels, key=rating_labels.get)

        general_tags = sorted(
            [(labels[i][0], labels[i][1]) for i in self.general_idxs if labels[i][1] > GENERAL_THRESHOLD],
            key=lambda x: x[1],
            reverse=True,
        )

        character_tags = sorted(
            [(labels[i][0], labels[i][1]) for i in self.character_idxs if labels[i][1] > CHARACTER_THRESHOLD],
            key=lambda x: x[1],
            reverse=True,
        )

        return general_tags, character_tags, top_rating, rating_labels


def build_response(image_path, tagger):
    """Run inference and return (tags, meta) dict."""
    if not image_path or not os.path.isfile(image_path):
        return {"tags": [], "meta": {"error": f"File not found: {image_path}"}}

    try:
        general_tags, character_tags, top_rating, rating_scores = tagger.predict(image_path)
    except Exception as e:
        return {"tags": [], "meta": {"error": str(e)}}

    tags = [f"rating:{top_rating}"]
    for name, _conf in character_tags:
        tags.append(f"character:{name}")
    for name, _conf in general_tags:
        tags.append(name)

    meta = {
        "model": MODEL_REPO,
        "rating_scores": {k: round(v, 4) for k, v in rating_scores.items()},
        "general_threshold": GENERAL_THRESHOLD,
        "character_threshold": CHARACTER_THRESHOLD,
        "tag_count": len(tags),
    }

    return {"tags": tags, "meta": meta}


def run_daemon():
    """Daemon mode: load model once, process NDJSON requests on stdin.

    Protocol (newline-delimited JSON):
      Request:  {"id": "uuid", "path": "/media/img.jpg"}
      Response: {"id": "uuid", "tags": [...], "meta": {...}}
    """
    # Load the model BEFORE signaling readiness — the host starts sending
    # requests as soon as it sees DAEMON_READY, and ONNX runtime may print
    # provider info to stdout during session creation which would corrupt
    # the NDJSON protocol stream.
    tagger = Tagger()

    sys.stderr.write("DAEMON_READY\n")
    sys.stderr.flush()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        try:
            request = json.loads(line)
        except json.JSONDecodeError as e:
            response = {"id": None, "tags": [], "meta": {"error": f"Invalid JSON: {e}"}}
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()
            continue

        req_id = request.get("id")
        image_path = request.get("path", "")

        result = build_response(image_path, tagger)
        result["id"] = req_id

        sys.stdout.write(json.dumps(result) + "\n")
        sys.stdout.flush()


def main():
    """Legacy single-shot mode: read full JSON from stdin, write response."""
    raw = sys.stdin.read()
    request = json.loads(raw)

    action = request.get("action", "tag")
    media_path = request.get("media_path", "")

    if action != "tag":
        json.dump({"tags": [], "meta": None}, sys.stdout)
        return

    tagger = Tagger()
    result = build_response(media_path, tagger)
    json.dump(result, sys.stdout)


if __name__ == "__main__":
    if "--daemon" in sys.argv:
        run_daemon()
    else:
        main()
