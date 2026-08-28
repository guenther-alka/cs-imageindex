mod config;
mod exif_read;
mod face;
mod vision;

use base64::Engine;
use clap::Parser;
use config::VisionConfig;
use std::io::Cursor;
use std::path::PathBuf;

/// cs-imageindex -- read all images in a folder, build an index with
/// location (EXIF GPS), a scene description (vision-capable LLM) and
/// recognized people (face matching against reference/<name>/*.jpg).
/// Standalone single-binary port of the original Python/OpenCV prototype
/// (cs_26.08.28) -- see README for the full background.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Folder to index (scanned recursively)
    #[arg(long)]
    folder: PathBuf,

    /// Output CSV path
    #[arg(long)]
    out: PathBuf,

    /// reference/<Name>/*.jpg folder for face matching (omit = no person
    /// recognition, "personen" column left empty)
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

fn main() {
    let args = Args::parse();

    if args.print_config_example {
        print!("{}", config::CONFIG_EXAMPLE);
        return;
    }

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

    let (yunet, sface) = if have_face_models {
        match (
            face::load_model(yunet_path.to_str().unwrap()),
            face::load_model(sface_path.to_str().unwrap()),
        ) {
            (Ok(y), Ok(s)) => (Some(y), Some(s)),
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
    for entry in walkdir(&args.folder) {
        if face::is_image(&entry) {
            files.push(entry);
        }
    }
    files.sort();
    println!("found {} image(s) under {}", files.len(), args.folder.display());

    let out_file = std::fs::File::create(&args.out).expect("cannot create output CSV");
    let mut w = csv::Writer::from_writer(out_file);
    w.write_record(["datei", "aufnahmedatum", "gps_lat", "gps_lon", "personen", "unbekannte_gesichter", "beschreibung"])
        .unwrap();

    for (i, path) in files.iter().enumerate() {
        let rel = path.strip_prefix(&args.folder).unwrap_or(path);
        println!("[{}/{}] {}", i + 1, files.len(), rel.display());

        let img = match image::open(path) {
            Ok(im) => im,
            Err(e) => {
                w.write_record([
                    rel.to_string_lossy().as_ref(),
                    "", "", "", "", "",
                    &format!("[read error: {e}]"),
                ])
                .unwrap();
                w.flush().ok();
                continue;
            }
        };

        let dt = exif_read::read_datetime(path);
        let (lat, lon) = exif_read::read_gps(path);

        let (matched_names, unmatched) = if let (Some(y), Some(s)) = (&yunet, &sface) {
            face::identify_faces(&img, y, s, &reference_people).unwrap_or_default()
        } else {
            (Vec::new(), 0)
        };

        let desc = if vision_active {
            let data_url = image_to_data_url(&img);
            if use_ollama {
                vision::describe_ollama(&data_url, &args.ollama, &args.ollama_model)
            } else {
                vision::describe_openai_compatible(&data_url, &vcfg)
            }
        } else {
            String::new()
        };

        w.write_record([
            rel.to_string_lossy().as_ref(),
            &dt,
            &lat.map(|v| format!("{v:.6}")).unwrap_or_default(),
            &lon.map(|v| format!("{v:.6}")).unwrap_or_default(),
            &matched_names.join(", "),
            &(if unmatched > 0 { unmatched.to_string() } else { String::new() }),
            &desc,
        ])
        .unwrap();
        w.flush().ok();
    }

    println!("done -- index written to {}", args.out.display());
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
