#!/usr/bin/env python3
"""
WD Eva02 Large Tagger v3 plugin for LightView.

Uses the SmilingWolf/wd-eva02-large-tagger-v3 ONNX model to predict
danbooru-style tags for images. The model and label files are downloaded
from HuggingFace on first run and cached locally.

Streaming protocol (newline-delimited JSON):
  Stdin (host → plugin):  one request per line
      {"action": "tag", "path": "/abs/path/img.jpg"}
  Stdout (plugin → host): one result per line, in any order
      {"path": "...", "tags": [...], "meta": {...}}
      {"path": "...", "tags": [], "error": "..."}

The plugin streams: requests are consumed as they arrive and results are
emitted as soon as each batch finishes. Hosts like lightview-worker bound
their download pipeline on results, so buffering stdin to EOF would deadlock.
Images are batched for parallel GPU inference; videos run one at a time
(each has its own per-frame loop). The host advertises the expected request
count in LIGHTVIEW_JOB_TOTAL for instance-pool sizing.
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
import pandas as pd
from PIL import Image

MODEL_REPO = "SmilingWolf/wd-eva02-large-tagger-v3"
MODEL_FILENAME = "model.onnx"
LABEL_FILENAME = "selected_tags.csv"

GENERAL_THRESHOLD = 0.35
CHARACTER_THRESHOLD = 0.85

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


def prepare_image(image, target_size):
    image = image.convert("RGBA")
    canvas = Image.new("RGBA", image.size, (255, 255, 255))
    canvas.alpha_composite(image)
    image = canvas.convert("RGB")

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
        sys.stderr.write(f"wd-tagger: providers = {active_providers}\n")
        sys.stderr.flush()

        input_shape = self.model.get_inputs()[0].shape  # [batch, H, W, C]
        _, height, width, _ = input_shape
        self.target_size = height
        self.input_name = self.model.get_inputs()[0].name
        self.label_name = self.model.get_outputs()[0].name

        # Detect whether the model accepts a dynamic batch dimension.
        # ONNX dynamic dims show up as strings (symbolic) or non-int.
        self.batch_supported = not isinstance(input_shape[0], int) or input_shape[0] != 1

    def _infer_one(self, image):
        arr = prepare_image(image, self.target_size)
        return self.model.run([self.label_name], {self.input_name: arr})[0][0].astype(float)

    def _infer_batch(self, images):
        """Run inference on N images at once; returns [N, num_labels]."""
        batch = np.concatenate([prepare_image(img, self.target_size) for img in images], axis=0)
        return self.model.run([self.label_name], {self.input_name: batch})[0]

    def _scores_to_tags(self, preds):
        labels = list(zip(self.tag_names, preds))
        rating_labels = {labels[i][0]: labels[i][1] for i in self.rating_idxs}
        top_rating = max(rating_labels, key=rating_labels.get)
        general_tags = sorted(
            [(labels[i][0], labels[i][1]) for i in self.general_idxs if labels[i][1] > GENERAL_THRESHOLD],
            key=lambda x: x[1], reverse=True,
        )
        character_tags = sorted(
            [(labels[i][0], labels[i][1]) for i in self.character_idxs if labels[i][1] > CHARACTER_THRESHOLD],
            key=lambda x: x[1], reverse=True,
        )
        return general_tags, character_tags, top_rating, rating_labels

    def _build_response(self, path, scores):
        general_tags, character_tags, top_rating, rating_scores = self._scores_to_tags(scores)
        tags = [f"rating:{top_rating}"]
        tags.extend(f"character:{name}" for name, _ in character_tags)
        tags.extend(name for name, _ in general_tags)
        meta = {
            "model": MODEL_REPO,
            "rating_scores": {k: round(float(v), 4) for k, v in rating_scores.items()},
            "general_threshold": GENERAL_THRESHOLD,
            "character_threshold": CHARACTER_THRESHOLD,
            "tag_count": len(tags),
        }
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
                sys.stderr.write(f"wd-tagger: batched inference failed ({e}), falling back\n")
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
        # for wd-eva02-large.
        size_mb = 1300
    return int(math.ceil(size_mb * VRAM_INFLATION)) + RUNTIME_OVERHEAD_MB


def decide_instance_count(num_images):
    """How many model instances to spin up, given workload + free VRAM."""
    forced = os.environ.get("WDTAGGER_INSTANCES")
    if forced:
        try:
            n = max(1, int(forced))
            sys.stderr.write(f"wd-tagger: instances={n} (forced via WDTAGGER_INSTANCES)\n")
            sys.stderr.flush()
            return n
        except ValueError:
            pass

    if num_images <= 1:
        return 1
    if not has_gpu_provider():
        sys.stderr.write("wd-tagger: no GPU provider available, using 1 instance (CPU fallback)\n")
        sys.stderr.flush()
        return 1

    free_mb = get_free_vram_mb()
    if free_mb is None:
        sys.stderr.write(
            "wd-tagger: could not measure free VRAM (non-NVIDIA/AMD GPU or vendor "
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
        f"wd-tagger: free_vram={free_mb}MB, per_instance≈{per_instance}MB, "
        f"images={num_images}, instances={n} (vram_cap={by_vram}, work_cap={by_work})\n"
    )
    sys.stderr.flush()
    return n


# Sentinel marking end-of-input on the work queue.
_EOF = object()


def spawn_stdin_reader():
    """Feed validated request paths into a bounded queue from a reader thread.

    Streaming (instead of reading stdin to EOF first) matters for remote
    hosts: lightview-worker keeps only a bounded number of downloaded files
    on disk and downloads more only as results are emitted, so a plugin that
    waits for EOF before tagging deadlocks the job.
    """
    work_q = queue.Queue(maxsize=256)

    def reader():
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                request = json.loads(line)
            except json.JSONDecodeError as e:
                emit({"path": "", "tags": [], "error": f"Invalid JSON: {e}"})
                continue
            path = request.get("path", "")
            action = request.get("action", "tag")
            if action != "tag":
                emit({"path": path, "tags": [], "error": f"Unknown action: {action}"})
                continue
            if not path or not os.path.isfile(path):
                emit({"path": path, "tags": [], "error": f"File not found: {path}"})
                continue
            work_q.put(path)
        work_q.put(_EOF)

    threading.Thread(target=reader, daemon=True).start()
    return work_q


def run_workers(taggers, work_q, batch_size):
    """Consume the path queue with one worker thread per tagger instance.

    Batches fill opportunistically: when the queue runs dry the partial batch
    is flushed immediately, because a remote host may be waiting on results
    before it downloads (and queues) more images.
    """

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
                item = work_q.get(timeout=0.25)
            except queue.Empty:
                flush()
                continue
            if item is _EOF:
                work_q.put(_EOF)  # release sibling workers
                break
            if is_video(item):
                flush()  # a video interrupts batching; keep the GPU batch clean
                emit(tagger.predict_video(item))
            else:
                buf.append(item)
                if len(buf) >= batch_size:
                    flush()
        flush()

    threads = [threading.Thread(target=worker, args=(t,), daemon=True) for t in taggers]
    for t in threads:
        t.start()
    for t in threads:
        t.join()


def main():
    # Start consuming stdin immediately — requests buffer in the queue
    # while the model downloads/loads below.
    work_q = spawn_stdin_reader()

    # Block on model download before any sizing decision. The VRAM heuristic
    # reads model.onnx's actual file size, so it must exist on disk first.
    # download_model() is idempotent — a no-op once files are cached.
    csv_path = os.path.join(PLUGIN_DIR, LABEL_FILENAME)
    model_path = os.path.join(PLUGIN_DIR, MODEL_FILENAME)
    if not os.path.isfile(csv_path) or not os.path.isfile(model_path):
        sys.stderr.write("wd-tagger: downloading model from HuggingFace (first run, this may take a while)...\n")
        sys.stderr.flush()
    download_model()

    # Size the pool without reading the whole stream: the host advertises the
    # request count via LIGHTVIEW_JOB_TOTAL; with an older host, fall back to
    # whatever has buffered so far.
    try:
        expected = int(os.environ.get("LIGHTVIEW_JOB_TOTAL", ""))
    except ValueError:
        expected = work_q.qsize()

    n_instances = decide_instance_count(expected)

    # Load instances sequentially to avoid thundering-herd VRAM allocation.
    taggers = []
    for i in range(n_instances):
        try:
            taggers.append(Tagger())
            if n_instances > 1:
                sys.stderr.write(f"wd-tagger: instance {i+1}/{n_instances} ready\n")
                sys.stderr.flush()
        except Exception as e:
            sys.stderr.write(f"wd-tagger: instance {i+1}/{n_instances} failed: {e}\n")
            sys.stderr.flush()
            if not taggers:
                raise
            break  # carry on with however many we got

    batch_size = max(1, int(os.environ.get("WDTAGGER_BATCH_SIZE", "8")))

    # Images batch on the worker pool; a video flushes the batch and runs
    # inline on whichever worker picked it up.
    run_workers(taggers, work_q, batch_size)


if __name__ == "__main__":
    main()
