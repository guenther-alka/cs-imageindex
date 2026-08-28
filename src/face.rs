// Face detection (YuNet) + alignment + embedding (SFace) + matching,
// reimplemented directly against the raw ONNX graphs via `tract` -- no
// OpenCV/cgo dependency. Decode formulas and the alignment template are
// taken verbatim from OpenCV's own C++ implementation (modules/objdetect/
// src/face_detect.cpp and face_recognize.cpp), confirmed via the upstream
// source rather than guessed, specifically so the numbers match what the
// Python/OpenCV prototype produced.

use image::{DynamicImage, GenericImageView, RgbImage};
use std::path::Path;
use tract_onnx::prelude::*;

pub type Model = TypedRunnableModel<TypedModel>;

const YUNET_SIZE: usize = 640;
const SFACE_SIZE: usize = 112;
const SCORE_THRESHOLD: f32 = 0.9; // OpenCV's FaceDetectorYN default
const NMS_THRESHOLD: f32 = 0.3; // OpenCV's FaceDetectorYN default
const STRIDES: [usize; 3] = [8, 16, 32];
// Standard 112x112 ArcFace/SFace alignment template (OpenCV
// face_recognize.cpp, getSimilarityTransformMatrix): left eye, right eye,
// nose tip, left mouth corner, right mouth corner.
const ARCFACE_TEMPLATE: [(f32, f32); 5] = [
    (38.2946, 51.6963),
    (73.5318, 51.5014),
    (56.0252, 71.7366),
    (41.5493, 92.3655),
    (70.7299, 92.2041),
];

/// Read the EXIF Orientation tag (1-8), defaulting to 1 (normal) if absent
/// or unreadable.
fn read_exif_orientation(path: &Path) -> u32 {
    let Ok(file) = std::fs::File::open(path) else { return 1 };
    let mut bufreader = std::io::BufReader::new(file);
    let Ok(exifreader) = exif::Reader::new().read_from_container(&mut bufreader) else {
        return 1;
    };
    exifreader
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .unwrap_or(1)
}

/// Apply the standard EXIF orientation transform (values 1-8) so pixel data
/// matches how the photo is actually meant to be viewed. The `image` crate
/// does NOT do this automatically on decode -- without it, any photo with a
/// non-1 orientation tag (extremely common on phone photos, e.g. portrait
/// shots or ones taken upside-down) is fed to YuNet rotated, and a rotated-
/// too-far face detector input can simply fail to find a face at all
/// (observed on a real 9248x6936 phone photo with orientation=3/180 degrees,
/// cs_26.08.28 -- 0 faces detected before this fix, 1 correctly detected and
/// matched after).
fn apply_exif_orientation(img: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// Open an image by sniffing its actual content, not its file extension.
/// `image::open()` picks the decoder from the path's extension, which
/// silently fails (and gets swallowed by callers' `let Ok(..) else continue`)
/// on a mislabeled file -- e.g. a "referenzphoto.jpg" that is actually PNG
/// data (observed on a real user-supplied reference photo, cs_26.08.28).
/// Also corrects EXIF orientation (see apply_exif_orientation above) --
/// without both fixes, a real phone photo can silently fail to yield any
/// face at all for reasons that have nothing to do with detection quality.
pub fn open_image(path: &Path) -> image::ImageResult<DynamicImage> {
    let reader = image::ImageReader::open(path).map_err(image::ImageError::IoError)?;
    let reader = reader.with_guessed_format().map_err(image::ImageError::IoError)?;
    let img = reader.decode()?;
    let orientation = read_exif_orientation(path);
    Ok(apply_exif_orientation(img, orientation))
}

pub fn load_model(path: &str) -> TractResult<Model> {
    tract_onnx::onnx()
        .model_for_path(path)?
        .into_optimized()?
        .into_runnable()
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub bbox: [f32; 4], // x, y, w, h in original image pixels
    pub landmarks: [(f32, f32); 5],
    pub score: f32,
}

/// Resize to fit inside YUNET_SIZE x YUNET_SIZE preserving aspect ratio,
/// pad the rest with black (letterbox) -- matches OpenCV's "pad_image"
/// pattern for the fixed-size ONNX export. Returns the padded RGB image
/// plus the scale and offsets needed to map detections back to original
/// image coordinates.
fn letterbox(img: &DynamicImage) -> (RgbImage, f32, u32, u32) {
    let (w, h) = img.dimensions();
    let scale = (YUNET_SIZE as f32 / w as f32).min(YUNET_SIZE as f32 / h as f32);
    let new_w = (w as f32 * scale).round() as u32;
    let new_h = (h as f32 * scale).round() as u32;
    let resized = img.resize_exact(new_w, new_h, image::imageops::FilterType::Triangle);
    let mut canvas = RgbImage::new(YUNET_SIZE as u32, YUNET_SIZE as u32); // black-filled
    let off_x = (YUNET_SIZE as u32 - new_w) / 2;
    let off_y = (YUNET_SIZE as u32 - new_h) / 2;
    image::imageops::overlay(&mut canvas, &resized.to_rgb8(), off_x as i64, off_y as i64);
    (canvas, scale, off_x, off_y)
}

/// Build an NCHW, BGR, unnormalized (0..255 f32) tensor -- matches OpenCV's
/// `dnn::blobFromImage(img)` called with no extra args (scalefactor=1.0,
/// mean=0, swapRB=false; OpenCV images are BGR already).
fn rgb_to_bgr_chw_tensor(img: &RgbImage) -> Tensor {
    let (w, h) = img.dimensions();
    let mut data = vec![0f32; 3 * h as usize * w as usize];
    let plane = h as usize * w as usize;
    for (x, y, px) in img.enumerate_pixels() {
        let idx = y as usize * w as usize + x as usize;
        data[0 * plane + idx] = px[2] as f32; // B
        data[1 * plane + idx] = px[1] as f32; // G
        data[2 * plane + idx] = px[0] as f32; // R
    }
    Tensor::from_shape(&[1, 3, h as usize, w as usize], &data).unwrap()
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let (ax1, ay1, ax2, ay2) = (a[0], a[1], a[0] + a[2], a[1] + a[3]);
    let (bx1, by1, bx2, by2) = (b[0], b[1], b[0] + b[2], b[1] + b[3]);
    let ix1 = ax1.max(bx1);
    let iy1 = ay1.max(by1);
    let ix2 = ax2.min(bx2);
    let iy2 = ay2.min(by2);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    let area_a = a[2].max(0.0) * a[3].max(0.0);
    let area_b = b[2].max(0.0) * b[3].max(0.0);
    let union = area_a + area_b - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

fn nms(mut dets: Vec<Detection>) -> Vec<Detection> {
    dets.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    let mut keep: Vec<Detection> = Vec::new();
    'outer: for d in dets {
        for k in &keep {
            if iou(&d.bbox, &k.bbox) > NMS_THRESHOLD {
                continue 'outer;
            }
        }
        keep.push(d);
    }
    keep
}

/// Detect faces in `img` (any size), returning boxes+landmarks in the
/// ORIGINAL image's pixel coordinates.
pub fn detect_faces(img: &DynamicImage, yunet: &Model) -> TractResult<Vec<Detection>> {
    let (padded, scale, off_x, off_y) = letterbox(img);
    let input = rgb_to_bgr_chw_tensor(&padded);
    let outputs = yunet.run(tvec!(input.into()))?;

    // Output order confirmed by inspecting the model: 3x cls[1], 3x obj[1],
    // 3x bbox[4], 3x landmark[10], one per stride (8, 16, 32).
    let mut candidates: Vec<Detection> = Vec::new();
    for (i, &stride) in STRIDES.iter().enumerate() {
        let cols = YUNET_SIZE / stride;
        let rows = YUNET_SIZE / stride;
        let cls = outputs[i].to_array_view::<f32>()?;
        let obj = outputs[3 + i].to_array_view::<f32>()?;
        let bbox = outputs[6 + i].to_array_view::<f32>()?;
        let kps = outputs[9 + i].to_array_view::<f32>()?;
        let cls = cls.as_slice().unwrap();
        let obj = obj.as_slice().unwrap();
        let bbox = bbox.as_slice().unwrap();
        let kps = kps.as_slice().unwrap();

        for r in 0..rows {
            for c in 0..cols {
                let idx = r * cols + c;
                let cls_s = cls[idx].clamp(0.0, 1.0);
                let obj_s = obj[idx].clamp(0.0, 1.0);
                let score = (cls_s * obj_s).sqrt();
                if score < SCORE_THRESHOLD {
                    continue;
                }
                let dx = bbox[idx * 4];
                let dy = bbox[idx * 4 + 1];
                let dw = bbox[idx * 4 + 2];
                let dh = bbox[idx * 4 + 3];
                let cx = (c as f32 + dx) * stride as f32;
                let cy = (r as f32 + dy) * stride as f32;
                let bw = dw.exp() * stride as f32;
                let bh = dh.exp() * stride as f32;
                let x1 = cx - bw / 2.0;
                let y1 = cy - bh / 2.0;

                let mut lms = [(0f32, 0f32); 5];
                for n in 0..5 {
                    let lx = (kps[idx * 10 + 2 * n] + c as f32) * stride as f32;
                    let ly = (kps[idx * 10 + 2 * n + 1] + r as f32) * stride as f32;
                    lms[n] = (lx, ly);
                }

                // map from padded/letterboxed 640x640 space back to original
                let unscale = |px: f32| (px - 0.0) / scale;
                let ux1 = (x1 - off_x as f32) / scale;
                let uy1 = (y1 - off_y as f32) / scale;
                let uw = bw / scale;
                let uh = bh / scale;
                let ulms = lms.map(|(x, y)| ((x - off_x as f32) / scale, (y - off_y as f32) / scale));
                let _ = unscale; // silence unused-closure warning on some toolchains

                candidates.push(Detection {
                    bbox: [ux1, uy1, uw, uh],
                    landmarks: ulms,
                    score,
                });
            }
        }
    }
    Ok(nms(candidates))
}

/// Solve the 2D similarity transform (rotation + uniform scale + translation)
/// that best maps `src` points onto `dst` points in a least-squares sense.
/// The model u = a*x - b*y + tx, v = b*x + a*y + ty is LINEAR in (a,b,tx,ty),
/// so this is solved via ordinary least squares (4x4 normal equations) --
/// mathematically equivalent to OpenCV's SVD-based Umeyama solution for this
/// well-posed (non-degenerate, no reflection needed) 5-point case, without
/// needing a general SVD routine.
fn similarity_transform(src: &[(f32, f32); 5], dst: &[(f32, f32); 5]) -> [f32; 4] {
    // Normal equations A^T A p = A^T r, accumulated directly (4x4 system).
    let mut ata = [[0f64; 4]; 4];
    let mut atr = [0f64; 4];
    for i in 0..5 {
        let (x, y) = (src[i].0 as f64, src[i].1 as f64);
        let (u, v) = (dst[i].0 as f64, dst[i].1 as f64);
        // row for u: [x, -y, 1, 0]
        let row_u = [x, -y, 1.0, 0.0];
        // row for v: [y, x, 0, 1]
        let row_v = [y, x, 0.0, 1.0];
        for a in 0..4 {
            for b in 0..4 {
                ata[a][b] += row_u[a] * row_u[b] + row_v[a] * row_v[b];
            }
            atr[a] += row_u[a] * u + row_v[a] * v;
        }
    }
    solve4(ata, atr).map(|v| v as f32)
}

/// Gaussian elimination with partial pivoting for a 4x4 system.
fn solve4(mut a: [[f64; 4]; 4], mut r: [f64; 4]) -> [f64; 4] {
    for col in 0..4 {
        let mut piv = col;
        for row in (col + 1)..4 {
            if a[row][col].abs() > a[piv][col].abs() {
                piv = row;
            }
        }
        a.swap(col, piv);
        r.swap(col, piv);
        let d = a[col][col];
        if d.abs() < 1e-12 {
            continue;
        }
        for row in (col + 1)..4 {
            let f = a[row][col] / d;
            for k in col..4 {
                a[row][k] -= f * a[col][k];
            }
            r[row] -= f * r[col];
        }
    }
    let mut x = [0f64; 4];
    for row in (0..4).rev() {
        let mut s = r[row];
        for k in (row + 1)..4 {
            s -= a[row][k] * x[k];
        }
        x[row] = if a[row][row].abs() < 1e-12 { 0.0 } else { s / a[row][row] };
    }
    x
}

/// Align a detected face to the canonical 112x112 ArcFace/SFace template
/// using its 5 landmarks, via inverse-mapped bilinear sampling (equivalent
/// to OpenCV's warpAffine at 112x112).
fn align_crop(img: &DynamicImage, landmarks: &[(f32, f32); 5]) -> RgbImage {
    let [a, b, tx, ty] = similarity_transform(landmarks, &ARCFACE_TEMPLATE);
    // Forward: u = a*x - b*y + tx ; v = b*x + a*y + ty
    // Inverse (since it's a similarity: rotation+scale is orthogonal up to
    // scale s^2 = a^2+b^2):
    let s2 = (a * a + b * b) as f32;
    let rgb = img.to_rgb8();
    let (sw, sh) = rgb.dimensions();
    let mut out = RgbImage::new(SFACE_SIZE as u32, SFACE_SIZE as u32);
    for oy in 0..SFACE_SIZE {
        for ox in 0..SFACE_SIZE {
            let u = ox as f32 - tx;
            let v = oy as f32 - ty;
            let x = (a * u + b * v) / s2;
            let y = (-b * u + a * v) / s2;
            out.put_pixel(ox as u32, oy as u32, bilinear_sample(&rgb, x, y, sw, sh));
        }
    }
    out
}

fn bilinear_sample(img: &RgbImage, x: f32, y: f32, w: u32, h: u32) -> image::Rgb<u8> {
    if x < 0.0 || y < 0.0 || x >= (w - 1) as f32 || y >= (h - 1) as f32 {
        return image::Rgb([0, 0, 0]);
    }
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let p00 = img.get_pixel(x0, y0);
    let p10 = img.get_pixel(x0 + 1, y0);
    let p01 = img.get_pixel(x0, y0 + 1);
    let p11 = img.get_pixel(x0 + 1, y0 + 1);
    let mut out = [0u8; 3];
    for c in 0..3 {
        let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
        let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        out[c] = (top * (1.0 - fy) + bot * fy).round() as u8;
    }
    image::Rgb(out)
}

/// Compute the 128-d SFace embedding for one already-aligned 112x112 face.
pub fn embed(aligned: &RgbImage, sface: &Model) -> TractResult<Vec<f32>> {
    let input = rgb_to_bgr_chw_tensor(aligned);
    let outputs = sface.run(tvec!(input.into()))?;
    let arr = outputs[0].to_array_view::<f32>()?;
    let mut v: Vec<f32> = arr.as_slice().unwrap().to_vec();
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    Ok(v)
}

pub fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

// OpenCV's FaceRecognizerSF.match() with FR_COSINE uses a built-in
// threshold of ~0.363 (its own tuned default for this exact model, not a
// figure invented here -- see the comment in the original Python prototype
// this ports).
pub const MATCH_THRESHOLD: f32 = 0.363;

pub struct ReferencePerson {
    pub name: String,
    pub embeddings: Vec<Vec<f32>>,
}

pub fn load_reference_people(refdir: &str, yunet: &Model, sface: &Model) -> Vec<ReferencePerson> {
    let mut people = Vec::new();
    let Ok(entries) = std::fs::read_dir(refdir) else { return people; };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let mut embeddings = Vec::new();
        if let Ok(photos) = std::fs::read_dir(&path) {
            for photo in photos.flatten() {
                let p = photo.path();
                let Some(ext) = p.extension().and_then(|e| e.to_str()) else { continue };
                if !["jpg", "jpeg", "png", "bmp"].contains(&ext.to_lowercase().as_str()) {
                    continue;
                }
                let Ok(img) = open_image(&p) else { continue };
                let Ok(dets) = detect_faces(&img, yunet) else { continue };
                if let Some(best) = dets.iter().max_by(|a, b| a.score.partial_cmp(&b.score).unwrap()) {
                    let aligned = align_crop(&img, &best.landmarks);
                    if let Ok(emb) = embed(&aligned, sface) {
                        embeddings.push(emb);
                    }
                }
            }
        }
        if embeddings.is_empty() {
            eprintln!("  reference: {} -- WARNING: no face detected in any photo, skipped", name);
        } else {
            people.push(ReferencePerson { name, embeddings });
        }
    }
    people
}

/// Detect + identify all faces in an image against the loaded reference
/// people. Returns (matched person names, count of unmatched/unknown faces).
pub fn identify_faces(
    img: &DynamicImage,
    yunet: &Model,
    sface: &Model,
    people: &[ReferencePerson],
) -> TractResult<(Vec<String>, usize)> {
    let dets = detect_faces(img, yunet)?;
    let mut matched = Vec::new();
    let mut unmatched = 0usize;
    for d in &dets {
        let aligned = align_crop(img, &d.landmarks);
        let emb = embed(&aligned, sface)?;
        let mut best: Option<(&str, f32)> = None;
        for person in people {
            for pe in &person.embeddings {
                let sim = cosine_sim(&emb, pe);
                if best.is_none() || sim > best.unwrap().1 {
                    best = Some((&person.name, sim));
                }
            }
        }
        match best {
            Some((name, sim)) if sim >= MATCH_THRESHOLD => matched.push(name.to_string()),
            _ => unmatched += 1,
        }
    }
    Ok((matched, unmatched))
}

pub fn is_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).as_deref(),
        Some("jpg") | Some("jpeg") | Some("png") | Some("bmp") | Some("tif") | Some("tiff")
    )
}
