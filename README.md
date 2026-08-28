# cs-imageindex

Index a folder of photos and write one CSV row per image with:

- `date_taken` — EXIF `DateTimeOriginal`
- `gps_lat` / `gps_lon` — EXIF GPS, decimal degrees (empty if the photo has none)
- `place` — reverse-geocoded place name (e.g. "München, Deutschland") from the
  GPS coordinates, via OpenStreetMap/Nominatim (best-effort, empty if no GPS
  or offline)
- `camera` — EXIF Make + Model (e.g. "samsung SM-G981B")
- `resolution` — pixel dimensions, e.g. "4032x3024"
- `orientation` — `landscape` / `portrait` / `square`
- `flash` — `yes` / `no` / empty if the EXIF Flash tag is absent
- `blur_score` — Laplacian-variance sharpness estimate (higher = sharper;
  relative to this tool's own scale, not an absolute standard)
- `phash` — 64-bit perceptual hash (hex) for near-duplicate/burst-shot
  detection (compare via Hamming distance)
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

Single static binary, no runtime to install. Face detection/recognition
(YuNet + SFace, both ONNX) runs through [`tract`](https://github.com/sonos/tract)
(pure Rust, no OpenCV/cgo/onnxruntime dependency) — the exact detection and
alignment math is a from-scratch port of OpenCV's own upstream C++
(`face_detect.cpp` / `face_recognize.cpp`), not a black box. `tags`/`ocr_text`/
`description` come from a single vision-LLM call per photo (no extra API
calls); `blur_score`/`phash`/`is_screenshot` are computed locally with no
network and no extra dependencies.

Reverse geocoding uses the public Nominatim API and respects its usage
policy (~1 request/second, descriptive User-Agent) — disable with
`--no-geocode` if you'd rather not make that network call.

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
```

## Models

`models/yunet.onnx` and `models/sface.onnx` are bundled in this repo,
taken as-is from the [OpenCV Zoo](https://github.com/opencv/opencv_zoo)
(`face_detection_yunet` and `face_recognition_sface`). Both are
redistributable under their own permissive licenses — see
`models/LICENSE-yunet.txt` (MIT, Shiqi Yu) and `models/LICENSE-sface.txt`
(Apache 2.0) — kept alongside the model files per their attribution terms.

## Building on illumos/OmniOS

See `illumos/cs-imageindex_omnios_1a.sh` (modeled on RustFS's
`rustfs_omnios_1a.sh` build script).

## Status

Proof-of-concept stage — validated on Linux (EXIF/GPS, vision descriptions,
and structurally the face pipeline via `tract`), not yet cross-checked
pixel-for-pixel against the original Python/OpenCV prototype on real photos,
and not yet built/tested on illumos. See `illumos/` and the project's
napp-it CS `howto.ai/cs-imageindex.info` doc for the full background.

## License

BSD 2-Clause — same as napp-it CS's other standalone tools (cs-sync,
cs-send, cs-freeze4snap, cs-sleeper, cs-aihelp). See `LICENSE`.
