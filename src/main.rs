mod config;
mod dedup;
mod exif_read;
mod face;
mod geocode;
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
    exif: exif_read::ExifData,
    resolution: String,
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
}

fn process_one(path: &PathBuf, ctx: &SharedCtx) -> RowRecord {
    let rel = path.strip_prefix(ctx.folder).unwrap_or(path);
    let rel_path = rel.to_string_lossy().to_string();

    let img = match face::open_image(path) {
        Ok(im) => im,
        Err(e) => {
            return RowRecord {
                rel_path,
                exif: exif_read::ExifData::default(),
                resolution: String::new(),
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
                error: Some(format!("[read error: {e}]")),
            };
        }
    };

    let exif = exif_read::read_all(path);

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
    let is_screenshot = if exif_read::is_screenshot_heuristic(&exif) { "yes" } else { "" };

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
        exif,
        resolution,
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

const CSV_HEADER: [&str; 19] = [
    "file", "date_taken", "gps_lat", "gps_lon", "place", "camera", "resolution", "orientation",
    "flash", "blur_score", "phash", "duplicate_group", "is_screenshot", "people",
    "unknown_faces", "face_count", "tags", "ocr_text", "description",
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

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in walkdir(&folder) {
        if face::is_image(&entry) {
            files.push(entry);
        }
    }
    files.sort();
    println!("found {} image(s) under {}", files.len(), folder.display());

    // --resume: skip files already present in an existing output CSV.
    let append_mode = args.resume && out.exists();
    if append_mode {
        if let Ok(mut rdr) = csv::Reader::from_path(&out) {
            let mut already_done = std::collections::HashSet::new();
            for record in rdr.records().flatten() {
                if let Some(f) = record.get(0) {
                    already_done.insert(f.to_string());
                }
            }
            let before = files.len();
            files.retain(|p| {
                let rel = p.strip_prefix(&folder).unwrap_or(p).to_string_lossy().to_string();
                !already_done.contains(&rel)
            });
            println!(
                "--resume: {} already indexed in {}, {} remaining",
                before - files.len(),
                out.display(),
                files.len()
            );
        } else {
            eprintln!("WARNING: --resume given but {} could not be read as CSV -- starting fresh", out.display());
        }
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

    let out_file = if append_mode {
        std::fs::OpenOptions::new().append(true).open(&out).expect("cannot open output CSV for append")
    } else {
        std::fs::File::create(&out).expect("cannot create output CSV")
    };
    let mut w = csv::Writer::from_writer(out_file);
    if !append_mode {
        w.write_record(CSV_HEADER).unwrap();
    }

    for (rec, &group) in rows.iter().zip(dup_groups.iter()) {
        if let Some(err) = &rec.error {
            let mut row = vec![rec.rel_path.clone()];
            row.extend(std::iter::repeat(String::new()).take(17));
            row.push(err.clone());
            w.write_record(&row).unwrap();
            continue;
        }
        let dup_str = if group != 0 && dup_sizes[group] > 1 { group.to_string() } else { String::new() };
        w.write_record([
            rec.rel_path.as_str(),
            &rec.exif.datetime,
            &rec.exif.gps_lat.map(|v| format!("{v:.6}")).unwrap_or_default(),
            &rec.exif.gps_lon.map(|v| format!("{v:.6}")).unwrap_or_default(),
            &rec.place,
            &rec.camera,
            &rec.resolution,
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
    }
    w.flush().ok();

    println!("done -- index written to {}", out.display());
}

fn walkdir(root: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}
