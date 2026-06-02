#!/usr/bin/env python3
"""
Camie Tagger v2 plugin for LightView.

Uses the Camais03/camie-tagger-v2 model — a 143M-parameter Vision Transformer
with an initial-prediction → top-K candidate-selection → cross-attention
refinement pipeline — in its ONNX build. It predicts danbooru-style tags across
a 70,527-tag vocabulary spanning general, character, copyright, artist, meta,
year, and rating categories. The model and tag-metadata file are downloaded
from HuggingFace on first run and cached locally.

Streaming protocol (newline-delimited JSON):
  Stdin (host -> plugin):  one request per line
      {"action": "tag", "path": "/abs/path/img.jpg"}
  Stdout (plugin -> host): one result per line, in any order
      {"path": "...", "tags": [...], "meta": {...}}
      {"path": "...", "tags": [], "error": "..."}

The plugin reads stdin to EOF, batches images for parallel GPU inference,
and runs videos one at a time (each video has its own per-frame loop).

Preprocessing matches the model card's onnx_inference.py: aspect-preserving
LANCZOS resize, center-pad to 512x512 with the ImageNet mean color, ImageNet
normalization, channels-first (NCHW) RGB input. The ONNX graph emits three
outputs; the second (refined logits) is the one used for tagging.
"""

import ctypes
import glob as _glob
import io
import json
import math
import queue
import shutil
import subprocess
import sys
import threading
import os

# Pre-load CUDA 12 shared libraries from pip-installed nvidia packages so
# onnxruntime's C++ dlopen() can find them. LD_LIBRARY_PATH set after process
# start is too late — load order matters. This block is a no-op on Windows
# (no `.so` files in the glob) and macOS (no CUDA); on those platforms ONNX
# picks up GPU runtimes through the standard DLL/dylib search path instead.
# Key off the running interpreter (sys.prefix), not a sibling `.venv`, so this
# works regardless of where the venv lives — a per-plugin venv, the shared
# `plugins/.venv`, or an absolute path rewritten at install time.
_site = os.path.join(sys.prefix, "lib")
_nvidia_lib_dirs = sorted(_glob.glob(
    os.path.join(_site, "python*", "site-packages", "nvidia", "*", "lib")
))
_CUDA_PRELOAD = [
    "libcudart.so*", "libnvrtc.so*", "libnvJitLink.so*", "libcublas.so*",
    "libcublasLt.so*", "libcufft.so*", "libcurand.so*", "libcudnn.so*",
]
for _pattern in _CUDA_PRELOAD:
    for _d in _nvidia_lib_dirs:
        for _path in sorted(_glob.glob(os.path.join(_d, _pattern))):
            if _path.endswith(".a"):
                continue
            try:
                ctypes.CDLL(_path, mode=ctypes.RTLD_GLOBAL)
            except OSError:
                pass

import huggingface_hub
import numpy as np
import onnxruntime as rt
from PIL import Image

MODEL_REPO = "Camais03/camie-tagger-v2"
MODEL_FILENAME = "camie-tagger-v2.onnx"
METADATA_FILENAME = "camie-tagger-v2-metadata.json"

# Default detection threshold. The model card publishes two tuned profiles:
#   macro-optimized (recall-leaning) = 0.492, micro-optimized = 0.614.
# We default to the macro profile and let the user override per category.
DEFAULT_THRESHOLD = 0.492
# Per-category thresholds. Characters/copyright/artist are higher-confidence
# signals, so we keep them stricter to limit false positives.
CATEGORY_THRESHOLDS = {
    "general": 0.492,
    "character": 0.75,
    "copyright": 0.75,
    "artist": 0.75,
    "meta": 0.492,
    "year": 0.492,
    "rating": 0.0,  # rating handled separately (argmax over the rating tags)
}

# Categories that are emitted with a "<category>:" tag prefix; "general" tags
# are emitted bare. "rating" is collapsed to a single best label.
PREFIXED_CATEGORIES = ("character", "copyright", "artist", "meta", "year")

# The model expects a fixed 512x512 input (overridable from metadata).
TARGET_SIZE = 512
# ImageNet normalization.
NORM_MEAN = np.array([0.485, 0.456, 0.406], dtype=np.float32)
NORM_STD = np.array([0.229, 0.224, 0.225], dtype=np.float32)
# Center-pad fill ≈ ImageNet mean color.
PAD_COLOR = (124, 116, 104)

VIDEO_EXTENSIONS = {".mp4", ".mov", ".avi", ".mkv", ".webm", ".m4v", ".wmv", ".flv"}
VIDEO_FRAME_SAMPLES = 5

# Kaomoji tags that should not have underscores replaced with spaces
KAOMOJIS = {
    "0_0", "(o)_(o)", "+_+", "+_-", "._.", "<o>_<o>", "<|>_<|>",
    "=_=", ">_<", "3_3", "6_9", ">_o", "@_@", "^_^", "o_o",
    "u_u", "x_x", "|_|", "||_||",
}

PLUGIN_DIR = os.path.dirname(os.path.abspath(__file__))


def download_model():
    meta_path = os.path.join(PLUGIN_DIR, METADATA_FILENAME)
    model_path = os.path.join(PLUGIN_DIR, MODEL_FILENAME)
    if not os.path.isfile(meta_path):
        downloaded = huggingface_hub.hf_hub_download(MODEL_REPO, METADATA_FILENAME)
        shutil.copy2(downloaded, meta_path)
    if not os.path.isfile(model_path):
        downloaded = huggingface_hub.hf_hub_download(MODEL_REPO, MODEL_FILENAME)
        shutil.copy2(downloaded, model_path)
    return meta_path, model_path


def _normalize_tag(name):
    return name.replace("_", " ") if name not in KAOMOJIS else name


def load_labels(meta_path):
    """Parse the nested metadata.json into ordered (name, category) arrays.

    Structure:
      metadata["dataset_info"]["tag_mapping"]["idx_to_tag"]      {"0": "tag", ...}
      metadata["dataset_info"]["tag_mapping"]["tag_to_category"] {"tag": "general", ...}
      metadata["model_info"]["img_size"]                          int
    """
    with open(meta_path, "r", encoding="utf-8") as f:
        metadata = json.load(f)

    tag_mapping = metadata["dataset_info"]["tag_mapping"]
    idx_to_tag = tag_mapping["idx_to_tag"]
    tag_to_category = tag_mapping["tag_to_category"]

    num_tags = len(idx_to_tag)
    names = [""] * num_tags
    raw_names = [""] * num_tags
    categories = [""] * num_tags
    for idx_str, raw in idx_to_tag.items():
        idx = int(idx_str)
        if idx >= num_tags:
            continue
        raw_names[idx] = raw
        names[idx] = _normalize_tag(raw)
        categories[idx] = tag_to_category.get(raw, "general")

    img_size = metadata.get("model_info", {}).get("img_size", TARGET_SIZE)
    return names, raw_names, categories, img_size


def prepare_image(image, target_size):
    """Aspect-preserving LANCZOS resize, center-pad to square, ImageNet-normalize.

    Returns an NCHW float32 array shaped [1, 3, target_size, target_size].
    """
    if image.mode != "RGB":
        # Composite transparency onto the pad color so alpha doesn't bleed black.
        if image.mode in ("RGBA", "LA", "P"):
            rgba = image.convert("RGBA")
            bg = Image.new("RGBA", rgba.size, PAD_COLOR + (255,))
            bg.alpha_composite(rgba)
            image = bg.convert("RGB")
        else:
            image = image.convert("RGB")

    w, h = image.size
    aspect = w / h if h else 1.0
    if aspect > 1:
        new_w = target_size
        new_h = max(1, int(round(target_size / aspect)))
    else:
        new_h = target_size
        new_w = max(1, int(round(target_size * aspect)))

    resized = image.resize((new_w, new_h), Image.LANCZOS)
    canvas = Image.new("RGB", (target_size, target_size), PAD_COLOR)
    canvas.paste(resized, ((target_size - new_w) // 2, (target_size - new_h) // 2))

    arr = np.asarray(canvas, dtype=np.float32) / 255.0
    arr = (arr - NORM_MEAN) / NORM_STD
    # HWC -> CHW, add batch dim -> NCHW
    arr = np.transpose(arr, (2, 0, 1))
    return np.expand_dims(arr, axis=0)


def sigmoid(x):
    return 1.0 / (1.0 + np.exp(-x))


def is_video(path):
    return os.path.splitext(path)[1].lower() in VIDEO_EXTENSIONS


def get_video_duration(path):
    try:
        out = subprocess.run(
            ["ffprobe", "-v", "error", "-show_entries", "format=duration",
             "-of", "default=noprint_wrappers=1:nokey=1", path],
            capture_output=True, text=True, timeout=30, check=True,
        )
        return float(out.stdout.strip())
    except (subprocess.SubprocessError, ValueError, FileNotFoundError):
        return None


def extract_frame(path, timestamp):
    try:
        proc = subprocess.run(
            ["ffmpeg", "-loglevel", "error", "-ss", f"{timestamp:.3f}",
             "-i", path, "-frames:v", "1", "-f", "image2pipe",
             "-vcodec", "png", "-"],
            capture_output=True, timeout=60,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    if proc.returncode != 0 or not proc.stdout:
        return None
    try:
        img = Image.open(io.BytesIO(proc.stdout))
        img.load()
        return img
    except Exception:
        return None


def sample_video_timestamps(duration, n):
    if duration < 1.0:
        return [duration / 2.0]
    start, end = duration * 0.05, duration * 0.95
    if n <= 1:
        return [(start + end) / 2.0]
    step = (end - start) / (n - 1)
    return [start + i * step for i in range(n)]


class Tagger:
    """Holds a loaded ONNX model and label data; processes images one or many at a time."""

    def __init__(self):
        meta_path, model_path = download_model()
        self.tag_names, self.raw_names, self.categories, meta_img_size = load_labels(meta_path)
        self.rating_idxs = [i for i, c in enumerate(self.categories) if c == "rating"]

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
        max_threads = int(os.environ.get("ONNX_THREADS", "1"))
        sess_options.intra_op_num_threads = max_threads
        sess_options.inter_op_num_threads = max_threads
        sess_options.execution_mode = rt.ExecutionMode.ORT_SEQUENTIAL

        # Redirect stdout during session creation: ONNX's C++ provider info
        # would otherwise corrupt our NDJSON output stream on fd 1.
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
        sys.stderr.write(f"camie-tagger: providers = {active_providers}\n")
        sys.stderr.flush()

        # Input is NCHW: [batch, channels, height, width].
        input_shape = self.model.get_inputs()[0].shape
        height = input_shape[2] if isinstance(input_shape[2], int) else meta_img_size
        self.target_size = height or TARGET_SIZE
        self.input_name = self.model.get_inputs()[0].name

        # The graph emits initial logits, refined logits, and selected candidates.
        # We score on the refined logits (output index 1 when >1 output exists).
        self.output_names = [o.name for o in self.model.get_outputs()]
        self.refined_idx = 1 if len(self.output_names) >= 2 else 0

        # Detect whether the model accepts a dynamic batch dimension.
        # ONNX dynamic dims show up as strings (symbolic) or non-int.
        self.batch_supported = not isinstance(input_shape[0], int) or input_shape[0] != 1

    def _run(self, arr):
        outputs = self.model.run(None, {self.input_name: arr})
        return outputs[self.refined_idx]

    def _infer_one(self, image):
        arr = prepare_image(image, self.target_size)
        logits = self._run(arr)[0].astype(float)
        return sigmoid(logits)

    def _infer_batch(self, images):
        """Run inference on N images at once; returns [N, num_labels] of probabilities."""
        batch = np.concatenate([prepare_image(img, self.target_size) for img in images], axis=0)
        logits = self._run(batch)
        return sigmoid(logits)

    def _scores_to_tags(self, preds):
        """Group thresholded predictions by category; each value is a sorted list."""
        by_category = {}
        for i, prob in enumerate(preds):
            category = self.categories[i]
            if category == "rating":
                continue  # handled via argmax below
            thr = CATEGORY_THRESHOLDS.get(category, DEFAULT_THRESHOLD)
            if prob > thr:
                by_category.setdefault(category, []).append((self.tag_names[i], prob))
        for category in by_category:
            by_category[category].sort(key=lambda x: x[1], reverse=True)
        return by_category

    def _top_rating(self, preds):
        if not self.rating_idxs:
            return None, {}
        scores = {self.raw_names[i]: float(preds[i]) for i in self.rating_idxs}
        top = max(scores, key=scores.get)
        return top, scores

    def _build_response(self, path, scores):
        by_category = self._scores_to_tags(scores)
        top_rating, rating_scores = self._top_rating(scores)

        tags = []
        if top_rating is not None:
            tags.append(f"rating:{top_rating}")
        for category in PREFIXED_CATEGORIES:
            for name, _ in by_category.get(category, []):
                tags.append(f"{category}:{name}")
        for name, _ in by_category.get("general", []):
            tags.append(name)

        meta = {
            "model": MODEL_REPO,
            "thresholds": CATEGORY_THRESHOLDS,
            "tag_count": len(tags),
        }
        if rating_scores:
            meta["rating_scores"] = {k: round(v, 4) for k, v in rating_scores.items()}
        return {"path": path, "tags": tags, "meta": meta}

    def predict_image(self, path):
        try:
            scores = self._infer_one(Image.open(path))
            return self._build_response(path, scores)
        except Exception as e:
            return {"path": path, "tags": [], "error": str(e)}

    def predict_image_batch(self, paths):
        """Batched inference. Falls back to per-image if the model rejects a batched tensor."""
        if len(paths) == 1 or not self.batch_supported:
            return [self.predict_image(p) for p in paths]

        # Open all images first; any that fail are reported individually.
        loaded = []
        results = [None] * len(paths)
        for i, p in enumerate(paths):
            try:
                loaded.append((i, Image.open(p)))
            except Exception as e:
                results[i] = {"path": p, "tags": [], "error": str(e)}

        if loaded:
            indices, images = zip(*loaded)
            try:
                scores_batch = self._infer_batch(list(images))
                for j, idx in enumerate(indices):
                    results[idx] = self._build_response(paths[idx], scores_batch[j].astype(float))
            except Exception as e:
                # The model may have advertised dynamic batch but rejected the call.
                # Disable batching for the remainder of the run and process per-image.
                sys.stderr.write(f"camie-tagger: batched inference failed ({e}), falling back\n")
                sys.stderr.flush()
                self.batch_supported = False
                for idx in indices:
                    results[idx] = self.predict_image(paths[idx])

        return results

    def predict_video(self, path):
        try:
            duration = get_video_duration(path)
            if not duration or duration <= 0:
                return {"path": path, "tags": [], "error": "Could not determine video duration"}
            timestamps = sample_video_timestamps(duration, VIDEO_FRAME_SAMPLES)
            score_max = None
            sampled = []
            for t in timestamps:
                frame = extract_frame(path, t)
                if frame is None:
                    continue
                scores = self._infer_one(frame)
                score_max = scores if score_max is None else np.maximum(score_max, scores)
                sampled.append(t)
            if not sampled:
                return {"path": path, "tags": [], "error": "Failed to decode any frames"}
            response = self._build_response(path, score_max)
            response["meta"]["video_frames_sampled"] = len(sampled)
            response["meta"]["video_frame_timestamps"] = [round(t, 2) for t in sampled]
            return response
        except Exception as e:
            return {"path": path, "tags": [], "error": str(e)}


_emit_lock = threading.Lock()


def emit(result):
    with _emit_lock:
        sys.stdout.write(json.dumps(result) + "\n")
        sys.stdout.flush()


# Per-instance VRAM is dominated by model weights + cuDNN workspaces + activations.
# We size as: model_file_size * INFLATION + RUNTIME_OVERHEAD_MB.
VRAM_INFLATION = 1.2
RUNTIME_OVERHEAD_MB = 800
# Leave headroom for the host app's own GPU usage (thumbnails, viewer transforms).
RESERVE_VRAM_MB = 2000
# Below this many images per *extra* instance, the model-load cost dominates.
MIN_IMAGES_PER_EXTRA_INSTANCE = 64
# Hard cap; more than 4 instances on one GPU rarely helps a transformer model.
MAX_INSTANCES = 4


# Any of these ONNX providers means we have a non-CPU device that *might* benefit
# from multi-instance. We only actually scale up if we can also measure free VRAM.
_GPU_PROVIDERS = {
    "CUDAExecutionProvider", "ROCMExecutionProvider", "MIGraphXExecutionProvider",
    "TensorrtExecutionProvider", "DmlExecutionProvider", "CoreMLExecutionProvider",
}


def has_gpu_provider():
    return bool(set(rt.get_available_providers()) & _GPU_PROVIDERS)


def _nvidia_free_vram_mb():
    """Works on Linux + Windows wherever the NVIDIA driver is installed."""
    try:
        out = subprocess.run(
            ["nvidia-smi", "--query-gpu=memory.free", "--format=csv,noheader,nounits"],
            capture_output=True, text=True, timeout=5, check=True,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    try:
        lines = [l.strip() for l in out.stdout.strip().splitlines() if l.strip()]
        device_idx = int(os.environ.get("CUDA_DEVICE", "0"))
        if device_idx < len(lines):
            return int(lines[device_idx])
    except ValueError:
        pass
    return None


def _rocm_free_vram_mb():
    """AMD ROCm probe (Linux only). rocm-smi's JSON shape varies by version,
    so we try a couple of the common keys."""
    if sys.platform != "linux":
        return None
    try:
        out = subprocess.run(
            ["rocm-smi", "--showmeminfo", "vram", "--json"],
            capture_output=True, text=True, timeout=5, check=True,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    try:
        data = json.loads(out.stdout)
    except json.JSONDecodeError:
        return None

    device_idx = int(os.environ.get("HIP_VISIBLE_DEVICES", "0").split(",")[0] or "0")
    info = data.get(f"card{device_idx}") or next(iter(data.values()), None)
    if not isinstance(info, dict):
        return None

    def _read(key):
        v = info.get(key)
        try:
            return int(v) if v is not None else None
        except (ValueError, TypeError):
            return None

    total = _read("VRAM Total Memory (B)") or _read("VRAM Total (B)")
    used = _read("VRAM Total Used Memory (B)") or _read("VRAM Used (B)")
    if total and used is not None:
        return max(0, (total - used) // (1024 * 1024))
    return None


def get_free_vram_mb():
    """Free VRAM in MB on the active GPU, or None if undetectable.

    Tries NVIDIA's nvidia-smi first (Linux + Windows), then AMD's rocm-smi
    (Linux). Other configurations — Apple Silicon (unified memory),
    Windows AMD/Intel via DirectML, etc. — fall through to None and the
    caller drops to a single instance, which is the safe default."""
    return _nvidia_free_vram_mb() or _rocm_free_vram_mb()


def estimate_per_instance_vram_mb():
    """Estimated VRAM per model instance, derived from the on-disk model size."""
    model_path = os.path.join(PLUGIN_DIR, MODEL_FILENAME)
    try:
        size_mb = os.path.getsize(model_path) / (1024 * 1024)
    except OSError:
        # Model not yet downloaded (first run); fall back to a conservative guess
        # for camie-tagger-v2 (~789 MB on disk).
        size_mb = 800
    return int(math.ceil(size_mb * VRAM_INFLATION)) + RUNTIME_OVERHEAD_MB


def decide_instance_count(num_images):
    """How many model instances to spin up, given workload + free VRAM."""
    forced = os.environ.get("CAMIE_TAGGER_INSTANCES")
    if forced:
        try:
            n = max(1, int(forced))
            sys.stderr.write(f"camie-tagger: instances={n} (forced via CAMIE_TAGGER_INSTANCES)\n")
            sys.stderr.flush()
            return n
        except ValueError:
            pass

    if num_images <= 1:
        return 1
    if not has_gpu_provider():
        sys.stderr.write("camie-tagger: no GPU provider available, using 1 instance (CPU fallback)\n")
        sys.stderr.flush()
        return 1

    free_mb = get_free_vram_mb()
    if free_mb is None:
        sys.stderr.write(
            "camie-tagger: could not measure free VRAM (non-NVIDIA/AMD GPU or vendor "
            "tools missing), using 1 instance\n"
        )
        sys.stderr.flush()
        return 1

    per_instance = estimate_per_instance_vram_mb()
    usable = max(0, free_mb - RESERVE_VRAM_MB)
    by_vram = max(1, usable // per_instance)
    by_work = max(1, (num_images + MIN_IMAGES_PER_EXTRA_INSTANCE - 1) // MIN_IMAGES_PER_EXTRA_INSTANCE)
    n = max(1, min(int(by_vram), by_work, MAX_INSTANCES))

    sys.stderr.write(
        f"camie-tagger: free_vram={free_mb}MB, per_instance≈{per_instance}MB, "
        f"images={num_images}, instances={n} (vram_cap={by_vram}, work_cap={by_work})\n"
    )
    sys.stderr.flush()
    return n


def run_image_workers(taggers, paths, batch_size):
    """Distribute image paths across instances via a shared queue."""
    if not paths:
        return

    work_q = queue.Queue()
    for p in paths:
        work_q.put(p)

    def worker(tagger):
        buf = []
        def flush():
            if not buf:
                return
            for r in tagger.predict_image_batch(buf):
                emit(r)
            buf.clear()

        while True:
            try:
                path = work_q.get_nowait()
            except queue.Empty:
                break
            buf.append(path)
            if len(buf) >= batch_size:
                flush()
        flush()

    threads = [threading.Thread(target=worker, args=(t,), daemon=True) for t in taggers]
    for t in threads:
        t.start()
    for t in threads:
        t.join()


def main():
    # Block on model download before any sizing decision. The VRAM heuristic
    # reads the model's actual file size, so it must exist on disk first.
    # download_model() is idempotent — a no-op once files are cached.
    meta_path = os.path.join(PLUGIN_DIR, METADATA_FILENAME)
    model_path = os.path.join(PLUGIN_DIR, MODEL_FILENAME)
    if not os.path.isfile(meta_path) or not os.path.isfile(model_path):
        sys.stderr.write("camie-tagger: downloading model from HuggingFace (first run, this may take a while)...\n")
        sys.stderr.flush()
    download_model()

    # Read all requests up front so we can size the worker pool to the workload.
    # Per-line cost is ~80B; even 100k images is ~8MB of memory.
    requests = []
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            requests.append(json.loads(line))
        except json.JSONDecodeError as e:
            emit({"path": "", "tags": [], "error": f"Invalid JSON: {e}"})

    image_paths = []
    video_paths = []
    for r in requests:
        path = r.get("path", "")
        action = r.get("action", "tag")
        if action != "tag":
            emit({"path": path, "tags": [], "error": f"Unknown action: {action}"})
            continue
        if not path or not os.path.isfile(path):
            emit({"path": path, "tags": [], "error": f"File not found: {path}"})
            continue
        (video_paths if is_video(path) else image_paths).append(path)

    n_instances = decide_instance_count(len(image_paths))

    # Load instances sequentially to avoid thundering-herd VRAM allocation.
    taggers = []
    for i in range(n_instances):
        try:
            taggers.append(Tagger())
            if n_instances > 1:
                sys.stderr.write(f"camie-tagger: instance {i+1}/{n_instances} ready\n")
                sys.stderr.flush()
        except Exception as e:
            sys.stderr.write(f"camie-tagger: instance {i+1}/{n_instances} failed: {e}\n")
            sys.stderr.flush()
            if not taggers:
                raise
            break  # carry on with however many we got

    batch_size = max(1, int(os.environ.get("CAMIE_TAGGER_BATCH_SIZE", "8")))

    # Images run on the worker pool; videos run sequentially on instance 0
    # afterwards (each video is its own per-frame loop and doesn't batch).
    run_image_workers(taggers, image_paths, batch_size)
    for vp in video_paths:
        emit(taggers[0].predict_video(vp))


if __name__ == "__main__":
    main()
