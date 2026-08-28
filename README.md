# cs-imageindex

## Project concept

Suggested folder layout, e.g.:

```
/tank/images
/tank/images/data/*         (originals)
/tank/images/index          (search index, e.g. index.csv)
/tank/images/selections/*   (e.g. selections/selection_1 -- images linked
                              from the "Image selections" menu)
```

Images/videos picked into a selection are collected in that folder and
directly reachable there, without touching or duplicating the originals.

Workflow:

1. Index all images and videos as a napp-it CS job (restartable, skips
   already-indexed images -- see `--resume` below).
2. Build a selection from the index (creates a folder of links to the
   matching images; originals are never modified).

## What it does

Index a folder of photos and videos and write one CSV row per media file
with:

- `media_type` — `image` / `raw` / `heic` / `video`
- `date_taken` — EXIF `DateTimeOriginal`
- `gps_lat` / `gps_lon` — EXIF GPS, decimal degrees (empty if the photo has none)
- `place` — reverse-geocoded place name (e.g. "München, Deutschland") from the
  GPS coordinates, via OpenStreetMap/Nominatim (best-effort, empty if no GPS
  or offline)
- `camera` — EXIF Make + Model (e.g. "samsung SM-G981B")
- `resolution` — pixel dimensions, e.g. "4032x3024"
- `duration` — video duration in seconds (empty for still images)
- `orientation` — `landscape` / `portrait` / `square`
- `flash` — `yes` / `no` / empty if the EXIF Flash tag is absent
- `blur_score` — Laplacian-variance sharpness estimate (higher = sharper;
  relative to this tool's own scale, not an absolute standard)
- `phash` — 64-bit perceptual hash (hex) for near-duplicate/burst-shot
  detection (compare via Hamming distance)
- `duplicate_group` — a shared group number for photos whose `phash` is
  within `--dedup-threshold` Hamming distance of each other (empty if the
  photo has no near-duplicate in this run)
- `is_screenshot` — `yes` if the photo has no camera Make/Model EXIF at all
  (heuristic — catches screenshots/downloaded images, not foolproof)
- `people` — comma-separated names matched against reference photos
- `unknown_faces` — count of detected-but-unmatched faces
- `face_count` — total faces detected (matched + unmatched)
- `tags` — comma-separated scene tags from the vision LLM (e.g. "outdoor,
  nature, road")
- `ocr_text` — any text the vision LLM could read in the photo (signs,
  documents, screens), empty if none
- `description` — a one-sentence scene description from a vision-capable LLM

## Supported formats

**Still images:** `.jpg` / `.jpeg`, `.png`, `.bmp`, `.tif` / `.tiff`
(matched by file content, not just the extension).

**RAW photos (v0.3):** `.cr2` / `.cr3` (Canon), `.nef` (Nikon), `.arw`
(Sony), `.dng` (Adobe/generic), `.orf` (Olympus), `.rw2` (Panasonic),
`.raf` (Fujifilm), `.pef` (Pentax), `.srw` (Samsung) — decoded with the
pure-Rust `rawloader`/`imagepipe` crates, no external tools or system
libraries required.

**HEIC/HEIF (v0.3):** `.heic`, `.heif` — the default photo format on
recent iPhones (H.265/HEVC-compressed, roughly half the file size of
JPEG at equal quality). Decoded via the bundled `ffmpeg` (its ISOBMFF
"mov" demuxer + built-in HEVC decoder) — no system `libheif` is needed.
One representative frame is extracted and run through the same
face/vision/quality pipeline as a still photo, and the EXIF metadata
(date, GPS, camera) is read directly from the HEIF file. If `ffmpeg` is
missing, HEIC/HEIF files are skipped like any other unreadable file.

How HEIC is provided, per OS:
- **illumos/OmniOS** — bundled ffmpeg in the release archive; works out
  of the box, no extra package.
- **Linux** — bundled ffmpeg; works out of the box.
- **macOS** — bundled ffmpeg; works out of the box.
- **Windows** — bundled ffmpeg; works out of the box.

All release archives bundle the static LGPL `ffmpeg`/`ffprobe` (see
`LICENSE-ffmpeg.txt`), so HEIC — like video — works everywhere without
any system library dependency.

**Video (v0.3):** `.mp4`, `.mov`, `.avi`, `.m4v`, `.mkv` — one
representative frame is extracted and run through the same face/vision/
quality pipeline as a still image, and container metadata (creation
date, duration, and GPS — including the ISO-6709 location tag iPhone/
QuickTime `.mov` files use) is read the same way EXIF is for photos.
The release archives bundle a minimal static LGPL `ffmpeg`/`ffprobe`
next to the binary (built from source by `ci/build-ffmpeg.sh`), so video
indexing works out of the box. As a fallback, `ffmpeg`/`ffprobe` on
`PATH` is used if the bundled copies are removed; if neither is found,
video files are skipped with a one-line note printed at startup, and
everything else still runs normally.

Single static binary for the still-image and RAW paths, no runtime to
install. Face detection/recognition
(YuNet + SFace, both ONNX) runs through [`tract`](https://github.com/sonos/tract)
(pure Rust, no OpenCV/cgo/onnxruntime dependency) — the exact detection and
alignment math is a from-scratch port of OpenCV's own upstream C++
(`face_detect.cpp` / `face_recognize.cpp`), not a black box. `tags`/`ocr_text`/
`description` come from a single vision-LLM call per photo (no extra API
calls); `blur_score`/`phash`/`is_screenshot` are computed locally with no
network and no extra dependencies.

Reverse geocoding uses the public Nominatim API and respects its usage
policy (~1 request/second, descriptive User-Agent, and an in-process cache
keyed to ~1km so repeat lookups near the same spot don't re-hit the
network) — disable with `--no-geocode` if you'd rather not make that
network call at all.

Photos are processed on a small worker-thread pool (`--threads`, default up
to 4) — EXIF/quality/face-detection work runs in parallel, and vision/
geocode network calls are safely shared across threads (geocoding is
globally rate-limited regardless of thread count; each thread gets its own
vision API connection). Interrupted a big run? `--resume` picks up where an
existing `--out` CSV left off instead of starting over.

## Usage

```
cs-imageindex --folder /path/to/photos --out index.csv \
    --refdir /path/to/reference --models-dir /path/to/models \
    --legacy-cfg /path/to/cs-aihelp.cfg
```

### All options

```
Options:
      --folder <FOLDER>              Folder to index (scanned recursively)
      --out <OUT>                    Output CSV path
      --refdir <REFDIR>              reference/<Name>/*.jpg folder for face matching
                                      (omit = no person recognition, "people" column
                                      left empty)
      --models-dir <MODELS_DIR>      Directory containing yunet.onnx and sface.onnx
                                      (default: next to this binary, in a "models"
                                      subfolder)
      --config <CONFIG>              Own standalone config file (provider/endpoint/
                                      model/api_key) -- see --print-config-example
      --legacy-cfg <LEGACY_CFG>      Legacy fallback: read endpoint2/model2/api_key2
                                      from an existing napp-it cs-aihelp.cfg (used only
                                      if --config / env vars / CLI don't already
                                      resolve a usable endpoint+model)
      --provider <PROVIDER>          Vision provider, informational only
                                      ("openai-compatible" | "ollama")
      --endpoint <ENDPOINT>          Vision API endpoint, e.g.
                                      https://api.deepseek.com/chat/completions
      --model <MODEL>                Vision model name, e.g.
                                      deepseek-v4-flash-vision-exp
      --api-key <API_KEY>            Vision API key
      --ollama <OLLAMA>              Use a local Ollama endpoint instead of a cloud
                                      provider, e.g. http://127.0.0.1:11434
      --ollama-model <OLLAMA_MODEL>  Ollama model name [default: llama3.2-vision]
      --no-vision                    Skip the scene-description step entirely
                                      (location + faces only)
      --no-geocode                   Skip reverse-geocoding GPS coordinates to a place
                                      name (no network calls to the public Nominatim/
                                      OpenStreetMap API)
      --no-dedup                     Skip near-duplicate/burst-shot grouping (saves an
                                      O(n^2) hash comparison pass on very large folders)
      --dedup-threshold <N>          Hamming-distance threshold for duplicate grouping
                                      (0-64, lower = stricter) [default: 6]
      --threads <N>                  Worker threads for the per-photo pipeline. 0 = auto
                                      (up to 4 by default, so as not to hammer a vision
                                      API/Nominatim with too much concurrency) [default: 0]
      --resume                       Resume an interrupted run: skip files already in an
                                      existing --out CSV and append new rows instead of
                                      overwriting (duplicate-group ids only apply within
                                      the newly processed batch, not across resumes)
      --print-config-example         Print an example --config file and exit
  -h, --help                         Print help
  -V, --version                      Print version
```

`--provider`/`--endpoint`/`--model`/`--api-key` (CLI flags) take priority
over the `CS_IMAGEINDEX_PROVIDER`/`_ENDPOINT`/`_MODEL`/`_API_KEY`/
`_MAX_TOKENS` environment variables, which take priority over `--config`
(own config file, format via `--print-config-example`), which takes priority
over `--legacy-cfg` (fallback only, used solely if nothing else resolved a
usable endpoint+model). If nothing resolves at all, vision is silently
skipped (same as `--no-vision`) — location, faces, and the local-only
metadata columns still run.

`--refdir` expects `<refdir>/<Name>/*.jpg` — one or more reference photos per
person. `--models-dir` must contain `yunet.onnx` and `sface.onnx` (bundled
in this repo, see Models below).

### Examples

```
# Local Ollama instead of a cloud provider
cs-imageindex --folder ./photos --out index.csv \
    --ollama http://127.0.0.1:11434 --ollama-model llama3.2-vision

# Own standalone config file
cs-imageindex --print-config-example > cs-imageindex.cfg
# edit cs-imageindex.cfg, then:
cs-imageindex --folder ./photos --out index.csv --config cs-imageindex.cfg

# No network calls at all (EXIF + faces only, no vision, no geocoding)
cs-imageindex --folder ./photos --out index.csv --no-vision --no-geocode

# Direct CLI credentials, no config file
cs-imageindex --folder ./photos --out index.csv \
    --provider openai-compatible \
    --endpoint https://api.deepseek.com/chat/completions \
    --model deepseek-v4-flash-vision-exp --api-key sk-...

# Resume an interrupted run (e.g. after a crash halfway through a big folder)
cs-imageindex --folder ./photos --out index.csv --resume

# More worker threads (e.g. a local Ollama instance that can take the load)
cs-imageindex --folder ./photos --out index.csv --threads 8 \
    --ollama http://127.0.0.1:11434

# Stricter duplicate grouping, or skip it entirely
cs-imageindex --folder ./photos --out index.csv --dedup-threshold 2
cs-imageindex --folder ./photos --out index.csv --no-dedup
```

## Running the test suite

```
cargo test
```

Covers the pure-logic parts that don't need models, a network, or sample
photos: vision-response parsing (`vision.rs`), the blur/hash quality
signals (`quality.rs`), duplicate grouping (`dedup.rs`), and the geocoding
cache key (`geocode.rs`).

## Models

`models/yunet.onnx` and `models/sface.onnx` are bundled in this repo,
taken as-is from the [OpenCV Zoo](https://github.com/opencv/opencv_zoo)
(`face_detection_yunet` and `face_recognition_sface`). Both are
redistributable under their own permissive licenses — see
`models/LICENSE-yunet.txt` (MIT, Shiqi Yu) and `models/LICENSE-sface.txt`
(Apache 2.0) — kept alongside the model files per their attribution terms.

## Building on illumos/OmniOS

See `illumos/cs-imageindex_omnios_1a.sh` (modeled on RustFS's
`rustfs_omnios_1a.sh` build script) — built and tested on real OmniOS
r151058j hardware, including v0.3's RAW/HEIC/video support.

RAW support needs nothing extra (pure Rust). HEIC support also needs
nothing extra: like video, HEIC/HEIF is decoded via the bundled
`ffmpeg`/`ffprobe` (see Supported formats above) — the illumos release
archive carries the same minimal static LGPL ffmpeg built on the
illumos build host with `ci/build-ffmpeg.sh`. The illumos binary is
therefore fully self-contained: no `libheif`, no ooce packages, no
rpath/LD_LIBRARY_PATH games. A system `ffmpeg` on `PATH` is only used
as a fallback if the bundled copies are removed.

## Continuous integration / releases

`.github/workflows/release.yml` builds Linux, Windows and macOS
(arm64 + x86_64) binaries on GitHub-hosted runners and attaches them to the
GitHub Release whenever a `v*` tag is pushed. illumos/OmniOS is
deliberately NOT part of this workflow: GitHub Actions' runner binary has
no illumos/SunOS build at all, so a self-hosted runner on real OmniOS
hardware isn't actually possible the way it is for the other three
platforms. The illumos binary is built by hand with
`illumos/cs-imageindex_omnios_1a.sh` and uploaded to the same release
separately (`gh release upload <tag> cs-imageindex-illumos.amd64.tar.gz`).

The CI-built Linux/Windows/macOS release archives bundle a minimal
static LGPL `ffmpeg`/`ffprobe` (built from source by `ci/build-ffmpeg.sh`),
so video indexing works out of the box; the illumos release archive
bundles the same. See `LICENSE-ffmpeg.txt` for the FFmpeg LGPL notice.

Release asset names follow the napp-it CS Tools convention
(`<asset>-<platform>.<arch>.tar.gz`, e.g. `cs-imageindex-linux.amd64.tar.gz`)
so the binaries can be installed from the napp-it CS "About > CS Tools
Download" menu into `data/cs_server/tools/cs-imageindex/<platform>.<arch>/`.

## Status

v0.3.0: RAW/HEIC/video support validated end-to-end on both Linux (.112)
and OmniOS/illumos (.189, real hardware) — synthetic HEIC (Linux) and
video (both platforms) test files correctly produce container/GPS
metadata, run through the quality/dedup pipeline, and cross-format
near-duplicate grouping works even between a JPEG/HEIC/video of the same
source photo. All release archives (Linux/Windows/macOS/illumos) bundle
a minimal static LGPL `ffmpeg`/`ffprobe`, so video AND HEIC indexing
work out of the box (verified on Linux and illumos).

v0.2.0 baseline: validated end-to-end on Linux (.112) and OmniOS/illumos
(.189, real hardware, first-attempt clean builds both times) — EXIF/GPS,
reverse geocoding, vision descriptions with structured tags/OCR, quality
signals, and a confirmed real face-name match (not just a structurally
plausible pipeline). See the project's napp-it CS
`howto.ai/cs-imageindex.info` doc for the full development history,
including the two real bugs found and fixed along the way
(image-format-by-extension, missing EXIF-orientation
handling).

## License

BSD 2-Clause — same as napp-it CS's other standalone tools (cs-sync,
cs-send, cs-freeze4snap, cs-sleeper, cs-aihelp). See `LICENSE`.
