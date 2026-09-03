mod config;
mod dedup;
mod exif_read;
mod face;
mod geocode;
mod media;
mod quality;
mod vision;

use base64::Engine;
use clap::Parser;
use config::VisionConfig;
use rayon::prelude::*;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// cs-imageindex -- read all images in a folder, build an index with
/// location (EXIF GPS + reverse-geocoded place name), a scene description
/// with tags/OCR text (vision-capable LLM), recognized people (face
/// matching against reference/<name>/*.jpg), near-duplicate/burst-shot
/// grouping, and a few cheap image-quality signals (blur estimate,
/// perceptual hash). Standalone single-binary port of the original
/// Python/OpenCV prototype (cs_26.08.28) -- see README for the full
/// background.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Folder to index (scanned recursively)
    #[arg(long, required_unless_present = "print_config_example")]
    folder: Option<PathBuf>,

    /// Output CSV path
    #[arg(long, required_unless_present = "print_config_example")]
    out: Option<PathBuf>,

    /// reference/<Name>/*.jpg folder for face matching (omit = no person
    /// recognition, "people" column left empty)
    #[arg(long, default_value = "")]
    refdir: String,

    /// Directory containing yunet.onnx and sface.onnx (default: next to
    /// this binary, in a "models" subfolder)
    #[arg(long)]
    models_dir: Option<PathBuf>,

    /// Own standalone config file (provider/endpoint/model/api_key) -- see
    /// --print-config-example
    #[arg(long, default_value = "")]
    config: String,

    /// Legacy fallback: read endpoint2/model2/api_key2 from an existing
    /// napp-it cs-aihelp.cfg (used only if --config / env vars / CLI don't
    /// already resolve a usable endpoint+model)
    #[arg(long, default_value = "")]
    legacy_cfg: String,

    #[arg(long, default_value = "")]
    provider: String,
    #[arg(long, default_value = "")]
    endpoint: String,
    #[arg(long, default_value = "")]
    model: String,
    #[arg(long, default_value = "")]
    api_key: String,

    /// Use a local Ollama endpoint instead of a cloud provider, e.g.
    /// http://127.0.0.1:11434
    #[arg(long, default_value = "")]
    ollama: String,
    #[arg(long, default_value = "llama3.2-vision")]
    ollama_model: String,

    /// Skip the scene-description step entirely (location + faces only)
    #[arg(long)]
    no_vision: bool,

    /// Skip reverse-geocoding GPS coordinates to a place name (no network
    /// calls to the public Nominatim/OpenStreetMap API)
    #[arg(long)]
    no_geocode: bool,

    /// Skip near-duplicate/burst-shot grouping (saves an O(n^2) hash
    /// comparison pass on very large folders)
    #[arg(long)]
    no_dedup: bool,

    /// Hamming-distance threshold for the perceptual-hash duplicate
    /// grouping (0-64, lower = stricter "same shot" match)
    #[arg(long, default_value_t = 6)]
    dedup_threshold: u32,

    /// Number of worker threads for the per-photo pipeline (EXIF, quality
    /// signals, face detection, vision/geocode calls). 0 = auto (up to 4 --
    /// kept modest by default so as not to hammer a vision API/Nominatim
    /// with too many concurrent requests; raise explicitly if your setup
    /// can take it, e.g. a local Ollama instance)
    #[arg(long, default_value_t = 0)]
    threads: usize,

    /// Resume an interrupted run: skip files already present in an
    /// existing --out CSV (matched by relative path) and append new rows
    /// instead of overwriting. Note: duplicate-group ids are computed only
    /// within the newly processed batch, not against rows from a previous
    /// run.
    #[arg(long)]
    resume: bool,

    /// Print an example --config file and exit
    #[arg(long)]
    print_config_example: bool,

    /// Comma-separated folder names to skip entirely during the scan
    /// (matched case-insensitively against a directory's own name, at any
    /// depth -- the whole subtree under a match is not walked and nothing
    /// under it appears in the output). Use this to keep an indexer's own
    /// output folders out of its own index, e.g.
    /// --exclude _index,_selections
    #[arg(long, default_value = "")]
    exclude: String,

    /// Rotate to a new numbered CSV file every N data rows instead of one
    /// unbounded --out file, to keep per-file size/parse cost bounded on
    /// very large collections. Chunk files are named
    /// "<out-stem>_NNN.csv" (zero-padded, e.g. index_001.csv, index_002.csv,
    /// ...) in the same directory as --out; --out itself is never written
    /// when rotation is active. 0 disables rotation (writes exactly to
    /// --out, the old single-file behavior).
    #[arg(long, default_value_t = 20000)]
    rotate_rows: usize,
}

fn resolve_vision_config(args: &Args) -> VisionConfig {
    let cli = VisionConfig {
        provider: args.provider.clone(),
        endpoint: args.endpoint.clone(),
        model: args.model.clone(),
        api_key: args.api_key.clone(),
        max_tokens: 0,
    };
    let env = VisionConfig::from_env();
    let own_file = if !args.config.is_empty() {
        VisionConfig::from_own_file(&args.config).unwrap_or_default()
    } else {
        VisionConfig::default()
    };
    let mut cfg = cli.merged_over(env).merged_over(own_file);
    if !cfg.is_usable() && !args.legacy_cfg.is_empty() {
        if let Ok(legacy) = VisionConfig::from_legacy_cs_aihelp(&args.legacy_cfg) {
            cfg = cfg.merged_over(legacy);
        }
    }
    if cfg.max_tokens == 0 {
        cfg.max_tokens = 600;
    }
    cfg
}

fn image_to_data_url(img: &image::DynamicImage) -> String {
    let mut small = img.clone();
    small = small.resize(1024, 1024, image::imageops::FilterType::Triangle);
    let mut buf = Vec::new();
    small
        .to_rgb8()
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Jpeg)
        .expect("jpeg encode");
    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
    format!("data:image/jpeg;base64,{}", b64)
}

/// Everything computed for one photo, ready to be written as one CSV row
/// (duplicate_group is filled in afterwards, once all phashes are known).
struct RowRecord {
    rel_path: String,
    media_type: &'static str,
    exif: exif_read::ExifData,
    resolution: String,
    duration: Option<f64>,
    orientation: &'static str,
    camera: String,
    flash: &'static str,
    blur: f32,
    phash: u64,
    is_screenshot: &'static str,
    place: String,
    matched_names: Vec<String>,
    unmatched: usize,
    face_count: usize,
    vres: vision::VisionResult,
    error: Option<String>,
}

struct SharedCtx<'a> {
    folder: &'a PathBuf,
    yunet: Option<Arc<face::Model>>,
    sface: Option<Arc<face::Model>>,
    reference_people: &'a [face::ReferencePerson],
    vision_active: bool,
    use_ollama: bool,
    ollama_endpoint: &'a str,
    ollama_model: &'a str,
    vcfg: &'a VisionConfig,
    geocode_active: bool,
    ffmpeg: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
}

fn process_one(path: &PathBuf, ctx: &SharedCtx) -> RowRecord {
    let rel = path.strip_prefix(ctx.folder).unwrap_or(path);
    let rel_path = rel.to_string_lossy().to_string();
    let kind = media::kind(path);
    let media_type = kind.label();

    let err_row = |rel_path: String, msg: String| RowRecord {
        rel_path,
        media_type,
        exif: exif_read::ExifData::default(),
        resolution: String::new(),
        duration: None,
        orientation: "",
        camera: String::new(),
        flash: "",
        blur: 0.0,
        phash: 0,
        is_screenshot: "",
        place: String::new(),
        matched_names: Vec::new(),
        unmatched: 0,
        face_count: 0,
        vres: vision::VisionResult::default(),
        error: Some(msg),
    };

    // Video: extract one representative frame + container metadata via
    // ffmpeg/ffprobe instead of decoding the file directly / reading EXIF.
    let (img, exif, duration) = if kind == media::MediaKind::Video {
        let (Some(ffmpeg), Some(ffprobe)) = (&ctx.ffmpeg, &ctx.ffprobe) else {
            return err_row(
                rel_path,
                "[skipped: ffmpeg/ffprobe not found -- video support needs both on PATH \
                 or bundled next to this binary, see README \"Supported formats\"]"
                    .to_string(),
            );
        };
        let meta = media::probe_video(path, ffprobe);
        let img = match media::extract_video_frame(path, ffmpeg, meta.duration_secs) {
            Ok(im) => im,
            Err(e) => return err_row(rel_path, format!("[video read error: {e}]")),
        };
        let mut exif = exif_read::ExifData::default();
        exif.datetime = meta.creation_time.clone().unwrap_or_default();
        exif.gps_lat = meta.gps.map(|(lat, _)| lat);
        exif.gps_lon = meta.gps.map(|(_, lon)| lon);
        (img, exif, meta.duration_secs)
    } else if kind == media::MediaKind::Heic {
        // HEIC/HEIF (e.g. iPhone photos): the bundled ffmpeg decodes these via
        // its ISOBMFF (mov) demuxer + built-in HEVC decoder -- no system
        // libheif needed. Extract one frame like video; EXIF metadata (date,
        // GPS, camera) is read directly from the HEIF file's Exif box.
        let Some(ffmpeg) = &ctx.ffmpeg else {
            return err_row(
                rel_path,
                "[skipped: ffmpeg not found -- HEIC support needs the bundled ffmpeg \
                 (see README \"Supported formats\")]"
                    .to_string(),
            );
        };
        let img = match media::extract_video_frame(path, ffmpeg, None) {
            Ok(im) => im,
            Err(e) => return err_row(rel_path, format!("[HEIC read error: {e}]")),
        };
        let exif = exif_read::read_all(path);
        (img, exif, None)
    } else {
        let img = match media::open_still_image(path) {
            Ok(im) => im,
            Err(e) => return err_row(rel_path, format!("[read error: {e}]")),
        };
        let exif = exif_read::read_all(path);
        (img, exif, None)
    };

    let (width, height) = (img.width(), img.height());
    let resolution = format!("{width}x{height}");
    let orientation = if width > height {
        "landscape"
    } else if height > width {
        "portrait"
    } else {
        "square"
    };
    let camera = format!("{} {}", exif.camera_make, exif.camera_model).trim().to_string();
    let flash = match exif.flash_fired {
        Some(true) => "yes",
        Some(false) => "no",
        None => "",
    };
    let blur = quality::blur_score(&img);
    let phash = quality::perceptual_hash(&img);
    // A video frame naturally has no camera Make/Model EXIF -- don't let
    // the screenshot heuristic (which relies on exactly that) misfire on it.
    let is_screenshot = if kind != media::MediaKind::Video && exif_read::is_screenshot_heuristic(&exif) {
        "yes"
    } else {
        ""
    };

    let place = match (exif.gps_lat, exif.gps_lon) {
        (Some(lat), Some(lon)) if ctx.geocode_active => geocode::reverse_geocode(lat, lon).unwrap_or_default(),
        _ => String::new(),
    };

    let (matched_names, unmatched) = if let (Some(y), Some(s)) = (&ctx.yunet, &ctx.sface) {
        face::identify_faces(&img, y, s, ctx.reference_people).unwrap_or_default()
    } else {
        (Vec::new(), 0)
    };
    let face_count = matched_names.len() + unmatched;

    let vres = if ctx.vision_active {
        let data_url = image_to_data_url(&img);
        if ctx.use_ollama {
            vision::describe_ollama(&data_url, ctx.ollama_endpoint, ctx.ollama_model)
        } else {
            vision::describe_openai_compatible(&data_url, ctx.vcfg)
        }
    } else {
        vision::VisionResult::default()
    };

    RowRecord {
        rel_path,
        media_type,
        exif,
        resolution,
        duration,
        orientation,
        camera,
        flash,
        blur,
        phash,
        is_screenshot,
        place,
        matched_names,
        unmatched,
        face_count,
        vres,
        error: None,
    }
}

const CSV_HEADER: [&str; 21] = [
    "file", "media_type", "date_taken", "gps_lat", "gps_lon", "place", "camera", "resolution",
    "duration", "orientation", "flash", "blur_score", "phash", "duplicate_group", "is_screenshot",
    "people", "unknown_faces", "face_count", "tags", "ocr_text", "description",
];

fn main() {
    let args = Args::parse();

    if args.print_config_example {
        print!("{}", config::CONFIG_EXAMPLE);
        return;
    }
    // clap guarantees these are Some() here (required_unless_present above).
    let folder = args.folder.clone().expect("--folder is required");
    let out = args.out.clone().expect("--out is required");

    let use_vision = !args.no_vision;
    let vcfg = resolve_vision_config(&args);
    let use_ollama = !args.ollama.is_empty();
    if use_vision && !use_ollama && !vcfg.is_usable() {
        eprintln!(
            "WARNING: no provider/endpoint/model resolved (checked CLI, env, --config, --legacy-cfg) -- \
             falling back to no-vision (location + faces only). Run --print-config-example for the config format."
        );
    }
    let vision_active = use_vision && (use_ollama || vcfg.is_usable());
    let geocode_active = !args.no_geocode;

    let models_dir = args.models_dir.clone().unwrap_or_else(|| {
        let mut d = std::env::current_exe().unwrap_or_default();
        d.pop();
        d.push("models");
        d
    });
    let yunet_path = models_dir.join("yunet.onnx");
    let sface_path = models_dir.join("sface.onnx");
    let have_face_models = yunet_path.exists() && sface_path.exists();
    if !have_face_models {
        eprintln!(
            "WARNING: {} / {} not found -- face detection/recognition disabled.",
            yunet_path.display(),
            sface_path.display()
        );
    }

    let (yunet, sface): (Option<Arc<face::Model>>, Option<Arc<face::Model>>) = if have_face_models {
        match (
            face::load_model(yunet_path.to_str().unwrap()),
            face::load_model(sface_path.to_str().unwrap()),
        ) {
            (Ok(y), Ok(s)) => (Some(Arc::new(y)), Some(Arc::new(s))),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("WARNING: failed to load face models ({e}) -- face detection disabled.");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    let reference_people = if !args.refdir.is_empty() {
        if let (Some(y), Some(s)) = (&yunet, &sface) {
            println!("loading reference people from {} ...", args.refdir);
            let people = face::load_reference_people(&args.refdir, y, s);
            if people.is_empty() {
                println!("  (no usable reference photos found -- person matching will report only face counts)");
            }
            people
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let excludes: Vec<String> = args
        .exclude
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if !excludes.is_empty() {
        println!("excluding folder(s) from scan: {}", excludes.join(", "));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in walkdir(&folder, &excludes) {
        if media::is_supported(&entry) {
            files.push(entry);
        }
    }
    files.sort();
    println!("found {} file(s) under {}", files.len(), folder.display());

    let ffmpeg = media::find_tool("ffmpeg");
    let ffprobe = media::find_tool("ffprobe");
    if ffmpeg.is_none() || ffprobe.is_none() {
        println!(
            "note: ffmpeg/ffprobe not found -- video files will be skipped \
             (see README \"Supported formats\")"
        );
    }

    let rotate = args.rotate_rows > 0;

    // --resume: skip files already present in an existing output CSV (or,
    // with rotation active, in ANY existing "<stem>_NNN.csv" chunk).
    let existing_chunks = if rotate { find_existing_chunks(&out) } else { Vec::new() };
    let append_mode = args.resume && (if rotate { !existing_chunks.is_empty() } else { out.exists() });
    // With rotation: row count already sitting in the last (highest-numbered)
    // chunk, so writing knows whether to keep filling it or start a fresh one.
    let mut last_chunk_rows: usize = 0;
    if append_mode {
        let scan_paths: Vec<PathBuf> = if rotate {
            existing_chunks.iter().map(|(_, p)| p.clone()).collect()
        } else {
            vec![out.clone()]
        };
        let mut already_done = std::collections::HashSet::new();
        let mut read_err = false;
        for (i, p) in scan_paths.iter().enumerate() {
            match csv::Reader::from_path(p) {
                Ok(mut rdr) => {
                    let mut n = 0usize;
                    for record in rdr.records().flatten() {
                        if let Some(f) = record.get(0) {
                            already_done.insert(f.to_string());
                        }
                        n += 1;
                    }
                    if rotate && i == scan_paths.len() - 1 {
                        last_chunk_rows = n;
                    }
                }
                Err(_) => read_err = true,
            }
        }
        if read_err {
            eprintln!("WARNING: --resume given but not every existing chunk could be read as CSV -- indexed-file list may be incomplete");
        }
        let before = files.len();
        files.retain(|p| {
            let rel = p.strip_prefix(&folder).unwrap_or(p).to_string_lossy().to_string();
            !already_done.contains(&rel)
        });
        println!(
            "--resume: {} already indexed ({} chunk file(s) under {}), {} remaining",
            before - files.len(),
            scan_paths.len(),
            out.parent().map(|p| p.display().to_string()).unwrap_or_default(),
            files.len()
        );
    }

    // Thread pool: default kept modest (<=4) so vision-API/Nominatim calls
    // don't get hammered with too much concurrency by default; --threads
    // raises or lowers it explicitly.
    let n_threads = if args.threads > 0 {
        args.threads
    } else {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(4)
    };
    println!("using {n_threads} worker thread(s)");
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n_threads)
        .build()
        .expect("failed to build thread pool");

    let ctx = SharedCtx {
        folder: &folder,
        yunet,
        sface,
        reference_people: &reference_people,
        vision_active,
        use_ollama,
        ollama_endpoint: &args.ollama,
        ollama_model: &args.ollama_model,
        vcfg: &vcfg,
        geocode_active,
        ffmpeg,
        ffprobe,
    };

    let total = files.len();
    let done = AtomicUsize::new(0);
    let rows: Vec<RowRecord> = pool.install(|| {
        files
            .par_iter()
            .map(|path| {
                let rec = process_one(path, &ctx);
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                println!("[{n}/{total}] {}", rec.rel_path);
                rec
            })
            .collect()
    });

    // Near-duplicate grouping over this run's batch (see --resume note in
    // help text: not merged with a previous run's rows).
    let dup_groups: Vec<usize> = if args.no_dedup {
        vec![0; rows.len()]
    } else {
        let hashes: Vec<u64> = rows.iter().map(|r| r.phash).collect();
        dedup::group_by_hash(&hashes, args.dedup_threshold)
    };
    let dup_sizes = dedup::group_sizes(&dup_groups);

    // --rotate-rows: figure out which chunk number/file to start writing
    // into. A --resume run continues filling the last existing chunk (in
    // append mode) if it still has room; otherwise (or without --resume)
    // rotation starts a fresh chunk family at 001, clearing any stale
    // chunk files left over from a differently-sized earlier run.
    let mut chunk_no: usize = 1;
    let mut rows_in_chunk: usize = 0;
    let mut continuing_existing = false;
    if rotate {
        if append_mode {
            let last_n = existing_chunks.last().map(|(n, _)| *n).unwrap_or(0);
            if last_n > 0 && last_chunk_rows < args.rotate_rows {
                chunk_no = last_n;
                rows_in_chunk = last_chunk_rows;
                continuing_existing = true;
            } else {
                chunk_no = last_n + 1;
            }
        } else {
            for (_, p) in &existing_chunks {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    let (mut w, mut cur_path) =
        open_chunk_writer(&out, rotate, chunk_no, if rotate { continuing_existing } else { append_mode });
    let mut chunks_written: Vec<PathBuf> = vec![cur_path.clone()];

    for (rec, &group) in rows.iter().zip(dup_groups.iter()) {
        if rotate && rows_in_chunk >= args.rotate_rows {
            w.flush().ok();
            chunk_no += 1;
            let (nw, np) = open_chunk_writer(&out, rotate, chunk_no, false);
            w = nw;
            cur_path = np;
            chunks_written.push(cur_path.clone());
            rows_in_chunk = 0;
        }

        if let Some(err) = &rec.error {
            let mut row = vec![rec.rel_path.clone(), rec.media_type.to_string()];
            row.extend(std::iter::repeat(String::new()).take(18));
            row.push(err.clone());
            w.write_record(&row).unwrap();
            rows_in_chunk += 1;
            continue;
        }
        let dup_str = if group != 0 && dup_sizes[group] > 1 { group.to_string() } else { String::new() };
        w.write_record([
            rec.rel_path.as_str(),
            rec.media_type,
            &rec.exif.datetime,
            &rec.exif.gps_lat.map(|v| format!("{v:.6}")).unwrap_or_default(),
            &rec.exif.gps_lon.map(|v| format!("{v:.6}")).unwrap_or_default(),
            &rec.place,
            &rec.camera,
            &rec.resolution,
            &rec.duration.map(|d| format!("{d:.1}")).unwrap_or_default(),
            rec.orientation,
            rec.flash,
            &format!("{:.1}", rec.blur),
            &format!("{:016x}", rec.phash),
            &dup_str,
            rec.is_screenshot,
            &rec.matched_names.join(", "),
            &(if rec.unmatched > 0 { rec.unmatched.to_string() } else { String::new() }),
            &(if rec.face_count > 0 { rec.face_count.to_string() } else { String::new() }),
            &rec.vres.tags,
            &rec.vres.ocr_text,
            &rec.vres.description,
        ])
        .unwrap();
        rows_in_chunk += 1;
    }
    w.flush().ok();

    if rotate {
        let names: Vec<String> = chunks_written
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .collect();
        println!(
            "done -- index written to {} chunk file(s) under {}: {}",
            chunks_written.len(),
            out.parent().map(|p| p.display().to_string()).unwrap_or_default(),
            names.join(", ")
        );
    } else {
        println!("done -- index written to {}", out.display());
    }

    write_facets(&out, rotate);
}

/// Aggregate the FULL merged index (every chunk file, or the single --out
/// file if rotation is off) into "<dir>/facets.csv" -- a small
/// (dimension,value,count) lookup table of the values actually present in
/// the index, for a UI (Media Selection) to offer as pick-lists instead of
/// free-text guessing. Recomputed from scratch every run (cheap relative to
/// indexing itself: just a re-parse of already-written CSV text), so it
/// always reflects the complete index, including rows a --resume run left
/// untouched. One row per --out family, so callers don't need to know
/// about chunking to find it.
fn write_facets(out: &std::path::Path, rotate: bool) {
    let paths: Vec<PathBuf> = if rotate {
        find_existing_chunks(out).into_iter().map(|(_, p)| p).collect()
    } else if out.exists() {
        vec![out.to_path_buf()]
    } else {
        Vec::new()
    };
    if paths.is_empty() {
        return;
    }

    const DIMS: [&str; 5] = ["media_type", "date_month", "place", "camera", "resolution"];
    let mut counts: std::collections::BTreeMap<&'static str, std::collections::HashMap<String, usize>> =
        DIMS.iter().map(|d| (*d, std::collections::HashMap::new())).collect();

    for p in &paths {
        let Ok(mut rdr) = csv::Reader::from_path(p) else { continue };
        for record in rdr.records().flatten() {
            let get = |i: usize| record.get(i).unwrap_or("").trim().to_string();
            let media_type = get(1);
            let date_taken = get(2);
            let place = get(5);
            let camera = get(6);
            let resolution = get(7);

            if !media_type.is_empty() {
                *counts.get_mut("media_type").unwrap().entry(media_type).or_insert(0) += 1;
            }
            if let Some(ym) = year_month(&date_taken) {
                *counts.get_mut("date_month").unwrap().entry(ym).or_insert(0) += 1;
            }
            if !place.is_empty() {
                *counts.get_mut("place").unwrap().entry(place).or_insert(0) += 1;
            }
            if !camera.is_empty() {
                *counts.get_mut("camera").unwrap().entry(camera).or_insert(0) += 1;
            }
            if !resolution.is_empty() {
                *counts.get_mut("resolution").unwrap().entry(resolution).or_insert(0) += 1;
            }
        }
    }

    let facets_path = out.parent().map(|p| p.join("facets.csv")).unwrap_or_else(|| PathBuf::from("facets.csv"));
    let f = match std::fs::File::create(&facets_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("WARNING: could not write {}: {e}", facets_path.display());
            return;
        }
    };
    let mut fw = csv::Writer::from_writer(f);
    fw.write_record(["dimension", "value", "count"]).ok();
    for (dim, values) in &counts {
        let mut items: Vec<(&String, &usize)> = values.iter().collect();
        // most-common first; alphabetical as a stable tiebreaker
        items.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (value, count) in items {
            fw.write_record([*dim, value.as_str(), &count.to_string()]).ok();
        }
    }
    fw.flush().ok();
    println!("facets written to {}", facets_path.display());
}

/// "2026:08:30 14:02:11" or "2026-08-30 14:02:11" (EXIF display_value()
/// formatting has varied historically) -> Some("2026-08") ; None if the
/// leading characters don't look like a date at all (empty/unparseable
/// EXIF date).
fn year_month(dt: &str) -> Option<String> {
    let b = dt.as_bytes();
    if b.len() >= 7
        && b[0..4].iter().all(|c| c.is_ascii_digit())
        && (b[4] == b'-' || b[4] == b':')
        && b[5..7].iter().all(|c| c.is_ascii_digit())
    {
        Some(format!("{}-{}", &dt[0..4], &dt[5..7]))
    } else {
        None
    }
}

// --rotate-rows: chunk file naming/discovery helpers.
//
// Chunk family for a given --out path (e.g. "_index/index.csv") is every
// "<dir>/<stem>_NNN.csv" (NNN = zero-padded number, 3+ digits). --out's own
// literal path is never written while rotation is active -- only the
// numbered chunks are.
fn chunk_stem(out: &std::path::Path) -> String {
    out.file_stem().and_then(|s| s.to_str()).unwrap_or("index").to_string()
}

fn chunk_path(out: &std::path::Path, n: usize) -> PathBuf {
    let dir = out.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    dir.join(format!("{}_{:03}.csv", chunk_stem(out), n))
}

/// Existing "<stem>_NNN.csv" chunk files next to --out, sorted ascending by
/// NNN. Returns (n, path) pairs.
fn find_existing_chunks(out: &std::path::Path) -> Vec<(usize, PathBuf)> {
    let dir = match out.parent() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let stem = chunk_stem(out);
    let prefix = format!("{stem}_");
    let mut found: Vec<(usize, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return found };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(&prefix) else { continue };
        let Some(num_part) = rest.strip_suffix(".csv") else { continue };
        if let Ok(n) = num_part.parse::<usize>() {
            found.push((n, e.path()));
        }
    }
    found.sort_by_key(|(n, _)| *n);
    found
}

/// Open (create, or append to an existing file) the CSV writer for one
/// output chunk. With rotation off, `path` is always the literal --out
/// path; with rotation on, it's chunk_path(out, chunk_no). A header row is
/// written only when NOT appending (fresh/new file).
fn open_chunk_writer(
    out: &std::path::Path,
    rotate: bool,
    chunk_no: usize,
    append: bool,
) -> (csv::Writer<std::fs::File>, PathBuf) {
    let path = if rotate { chunk_path(out, chunk_no) } else { out.to_path_buf() };
    let f = if append {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("cannot open output CSV for append")
    } else {
        std::fs::File::create(&path).expect("cannot create output CSV")
    };
    let mut w = csv::Writer::from_writer(f);
    if !append {
        w.write_record(CSV_HEADER).unwrap();
    }
    (w, path)
}

fn walkdir(root: &PathBuf, excludes: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.clone(), 0usize)];
    const MAX_DEPTH: usize = 64;   // audit B1: bound pathological trees
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            // file_type() is lstat-based (does NOT follow symlinks)
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !ft.is_symlink() {
                // --exclude: skip this directory's whole subtree (matched
                // case-insensitively by directory name, at any depth) --
                // e.g. an indexer's own _index/_selections output folders.
                if !excludes.is_empty() {
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        if excludes.iter().any(|ex| ex == &name.to_lowercase()) {
                            continue;
                        }
                    }
                }
                // real directory -> recurse. Directory symlinks are NOT
                // followed: a symlink cycle (dir -> ancestor) would otherwise
                // grow the walk stack without bound, and a symlink escaping
                // the root would index content outside the scanned folder.
                stack.push((p, depth + 1));
            } else if ft.is_file() {
                out.push(p);
            } else if ft.is_symlink() {
                // file symlink -> index the link (leaf node, no recursion
                // risk); directory symlink -> skip entirely.
                match std::fs::metadata(&p) {
                    Ok(m) if m.is_dir() => (),
                    _ => out.push(p),
                }
            }
        }
    }
    out
}
