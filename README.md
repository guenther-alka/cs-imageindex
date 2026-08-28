# cs-imageindex

Index a folder of photos and write one CSV row per image with:

- `date_taken` — EXIF `DateTimeOriginal`
- `gps_lat` / `gps_lon` — EXIF GPS, decimal degrees (empty if the photo has none)
- `description` — a one-sentence scene description from a vision-capable LLM
  (indoor/outdoor, type of place, what's happening)
- `people` — comma-separated names matched against reference photos
- `unknown_faces` — count of detected-but-unmatched faces

Single static binary, no runtime to install. Face detection/recognition
(YuNet + SFace, both ONNX) runs through [`tract`](https://github.com/sonos/tract)
(pure Rust, no OpenCV/cgo/onnxruntime dependency) — the exact detection and
alignment math is a from-scratch port of OpenCV's own upstream C++
(`face_detect.cpp` / `face_recognize.cpp`), not a black box.

## Usage

```
cs-imageindex --folder /path/to/photos --out index.csv \
    --refdir /path/to/reference --models-dir /path/to/models \
    --legacy-cfg /path/to/cs-aihelp.cfg
```

Vision-LLM provider/model/API key can come from (highest priority first):
CLI flags (`--provider/--endpoint/--model/--api-key`), environment variables
(`CS_IMAGEINDEX_PROVIDER/_ENDPOINT/_MODEL/_API_KEY/_MAX_TOKENS`), an own
config file (`--config`, run `--print-config-example` for the format), or as
a last resort `--legacy-cfg` pointing at an existing napp-it CS
`cs-aihelp.cfg` (reads its `endpoint2/model2/api_key2` slot). A local Ollama
instance can be used instead via `--ollama <endpoint>`. If nothing resolves,
vision is silently skipped (same as `--no-vision`) — location and face
matching still run.

`--refdir` expects `<refdir>/<Name>/*.jpg` — one or more reference photos per
person. `--models-dir` must contain `yunet.onnx` and `sface.onnx`.

## Models

Not included in this repo (binary/license reasons) — get them from the
OpenCV Zoo:

- https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet
- https://github.com/opencv/opencv_zoo/tree/main/models/face_recognition_sface

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

MIT
