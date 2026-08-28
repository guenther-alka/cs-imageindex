// v0.3: broader input-format support beyond plain JPEG/PNG/BMP/TIFF.
//
// Three additions, each with a different cost/tradeoff (see README
// "Supported formats" for the user-facing version of this):
//   - RAW photos (CR2/CR3/NEF/ARW/DNG/ORF/RW2/RAF/PEF/SRW): decoded via
//     rawloader + imagepipe, both pure Rust -- no system libraw/dcraw
//     dependency, keeps the "single static binary" property intact.
//   - HEIC/HEIF: decoded via the bundled ffmpeg (its ISOBMFF/mov demuxer +
//     built-in HEVC decoder) -- no system libheif dependency. EXIF metadata
//     is read directly from the HEIF file via kamadak-exif. If ffmpeg is
//     missing, HEIC files are skipped like any other unreadable file.
//   - Video (MP4/MOV/AVI/M4V/MKV): a representative frame is extracted via
//     an external `ffmpeg` binary (shelled out to, not linked) and container
//     metadata (creation time, duration, GPS if present) via `ffprobe`.
//     This is a genuine new runtime dependency -- if ffmpeg/ffprobe aren't
//     found, video files are skipped with a clear per-file error message
//     instead of a crash.

use crate::face;
use image::DynamicImage;
use std::path::{Path, PathBuf};
use std::process::Command;

const RAW_EXTS: &[&str] = &["cr2", "cr3", "nef", "arw", "dng", "orf", "rw2", "raf", "pef", "srw"];
const HEIC_EXTS: &[&str] = &["heic", "heif"];
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "avi", "m4v", "mkv"];

fn ext_lower(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

pub fn is_raw(path: &Path) -> bool {
    RAW_EXTS.contains(&ext_lower(path).as_str())
}

pub fn is_heic(path: &Path) -> bool {
    HEIC_EXTS.contains(&ext_lower(path).as_str())
}

pub fn is_video(path: &Path) -> bool {
    VIDEO_EXTS.contains(&ext_lower(path).as_str())
}

/// Any file this build of cs-imageindex can attempt to process at all.
pub fn is_supported(path: &Path) -> bool {
    face::is_image(path) || is_raw(path) || is_heic(path) || is_video(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Raw,
    Heic,
    Video,
}

pub fn kind(path: &Path) -> MediaKind {
    if is_video(path) {
        MediaKind::Video
    } else if is_raw(path) {
        MediaKind::Raw
    } else if is_heic(path) {
        MediaKind::Heic
    } else {
        MediaKind::Image
    }
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Raw => "raw",
            MediaKind::Heic => "heic",
            MediaKind::Video => "video",
        }
    }
}

/// Decode a RAW photo into a standard RGB image via rawloader (sensor/
/// Bayer decode) + imagepipe (demosaic, white balance, gamma) -- pure
/// Rust, no libraw/dcraw system dependency.
pub fn open_raw(path: &Path) -> Result<DynamicImage, String> {
    let srgb = imagepipe::simple_decode_8bit(path, 0, 0)
        .map_err(|e| format!("RAW decode failed: {e}"))?;
    image::RgbImage::from_raw(srgb.width as u32, srgb.height as u32, srgb.data)
        .map(DynamicImage::ImageRgb8)
        .ok_or_else(|| "RAW decode: pixel buffer/size mismatch".to_string())
}

/// Open any supported still-image format -- JPEG/PNG/BMP/TIFF (via the
/// `image` crate, with EXIF-orientation correction) or RAW. HEIC/HEIF is
/// decoded via the bundled ffmpeg in process_one (main.rs), not here.
pub fn open_still_image(path: &Path) -> Result<DynamicImage, String> {
    match kind(path) {
        MediaKind::Raw => open_raw(path),
        MediaKind::Heic => Err("HEIC: decoded via the bundled ffmpeg (not the image crate)".to_string()),
        _ => face::open_image(path).map_err(|e| e.to_string()),
    }
}

/// Locate ffmpeg/ffprobe: prefer a copy bundled next to this executable
/// (a self-contained release, same idea as the bundled ONNX models), else
/// fall back to PATH. Returns None if neither responds to `-version`, so
/// callers can skip video processing cleanly instead of failing.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let fname = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
            let bundled = dir.join(&fname);
            if bundled.is_file() {
                return Some(bundled);
            }
        }
    }
    Command::new(name)
        .arg("-version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| PathBuf::from(name))
}

#[derive(Debug, Default, Clone)]
pub struct VideoMeta {
    pub creation_time: Option<String>,
    pub duration_secs: Option<f64>,
    pub gps: Option<(f64, f64)>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Read container metadata (creation time, duration, resolution, and GPS
/// if present as an ISO-6709 location tag -- the format iPhone/QuickTime
/// .mov files use) via ffprobe. Best-effort: any parse failure just leaves
/// the corresponding field empty rather than erroring the whole file out.
pub fn probe_video(path: &Path, ffprobe: &Path) -> VideoMeta {
    let mut meta = VideoMeta::default();
    let Ok(out) = Command::new(ffprobe)
        .args(["-v", "quiet", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
    else {
        return meta;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return meta;
    };

    let format = &json["format"];
    meta.duration_secs = format["duration"].as_str().and_then(|s| s.parse().ok());
    meta.creation_time = format["tags"]["creation_time"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            json["streams"].as_array().and_then(|streams| {
                streams.iter().find_map(|s| s["tags"]["creation_time"].as_str().map(str::to_string))
            })
        });

    let loc = format["tags"]["com.apple.quicktime.location.ISO6709"]
        .as_str()
        .or_else(|| format["tags"]["location"].as_str());
    if let Some(loc) = loc {
        meta.gps = parse_iso6709(loc);
    }

    if let Some(streams) = json["streams"].as_array() {
        if let Some(v) = streams.iter().find(|s| s["codec_type"] == "video") {
            meta.width = v["width"].as_u64().map(|w| w as u32);
            meta.height = v["height"].as_u64().map(|h| h as u32);
        }
    }
    meta
}

/// Parse an ISO-6709 location string, e.g. "+48.1351+011.5820/" into
/// (lat, lon). Tries the whole remainder as the longitude first, then
/// progressively shorter suffixes -- some encoders append altitude with no
/// separator, which would otherwise fail a plain split-and-parse.
fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    let s = s.trim_end_matches('/');
    let bytes = s.as_bytes();
    let split = bytes.iter().enumerate().skip(1).find(|(_, b)| **b == b'+' || **b == b'-')?.0;
    let lat: f64 = s[..split].parse().ok()?;
    let lon_str = &s[split..];
    let lon: f64 = lon_str
        .parse()
        .ok()
        .or_else(|| (1..lon_str.len()).rev().find_map(|end| lon_str[..end].parse().ok()))?;
    Some((lat, lon))
}

/// Extract one representative frame (2s in for anything longer than 4s,
/// otherwise the first frame) as a JPEG via ffmpeg, then decode it with the
/// `image` crate so it can go through the same face/vision/quality pipeline
/// as a still photo.
pub fn extract_video_frame(
    path: &Path,
    ffmpeg: &Path,
    duration_secs: Option<f64>,
) -> Result<DynamicImage, String> {
    let seek = match duration_secs {
        Some(d) if d > 4.0 => "2".to_string(),
        _ => "0".to_string(),
    };
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("cs-imageindex-frame-{}-{suffix}.jpg", std::process::id()));

    let result = Command::new(ffmpeg)
        .args(["-y", "-ss", &seek, "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-q:v", "3"])
        .arg(&tmp)
        .output();

    let decoded = match result {
        Ok(out) if out.status.success() && tmp.is_file() => image::ImageReader::open(&tmp)
            .map_err(|e| e.to_string())
            .and_then(|r| r.with_guessed_format().map_err(|e| e.to_string()))
            .and_then(|r| r.decode().map_err(|e| e.to_string())),
        Ok(out) => Err(format!("ffmpeg frame extraction failed: {}", String::from_utf8_lossy(&out.stderr))),
        Err(e) => Err(format!("ffmpeg not runnable: {e}")),
    };
    let _ = std::fs::remove_file(&tmp);
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso6709_basic() {
        assert_eq!(parse_iso6709("+48.1351+011.5820/"), Some((48.1351, 11.5820)));
    }

    #[test]
    fn parses_iso6709_negative_lon() {
        assert_eq!(parse_iso6709("+40.7128-074.0060/"), Some((40.7128, -74.0060)));
    }

    #[test]
    fn parses_iso6709_with_altitude_suffix() {
        // Some encoders append altitude directly with no separator before
        // the trailing slash, e.g. "+48.1351+011.5820+123.4/".
        let (lat, lon) = parse_iso6709("+48.1351+011.5820+123.4/").unwrap();
        assert!((lat - 48.1351).abs() < 1e-6);
        assert!((lon - 11.5820).abs() < 1e-6);
    }

    #[test]
    fn kind_detects_extensions() {
        assert_eq!(kind(Path::new("a.CR2")), MediaKind::Raw);
        assert_eq!(kind(Path::new("a.heic")), MediaKind::Heic);
        assert_eq!(kind(Path::new("a.mp4")), MediaKind::Video);
        assert_eq!(kind(Path::new("a.jpg")), MediaKind::Image);
    }
}
