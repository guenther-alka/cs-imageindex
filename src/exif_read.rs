use exif::{In, Tag, Value};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// All EXIF fields cs-imageindex cares about, read in a single pass (one
/// file open/parse instead of one per field).
#[derive(Debug, Default, Clone)]
pub struct ExifData {
    pub datetime: String,
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    pub camera_make: String,
    pub camera_model: String,
    /// Some(true)/Some(false) if the Flash tag was present, None if absent.
    pub flash_fired: Option<bool>,
}

fn clean(s: &str) -> String {
    s.trim_matches('"').trim().to_string()
}

fn dms_to_deg(field: &exif::Field) -> Option<f64> {
    if let Value::Rational(ref v) = field.value {
        if v.len() == 3 {
            let deg = v[0].to_f64();
            let min = v[1].to_f64();
            let sec = v[2].to_f64();
            return Some(deg + min / 60.0 + sec / 3600.0);
        }
    }
    None
}

pub fn read_all(path: &Path) -> ExifData {
    let mut data = ExifData::default();
    let Ok(file) = File::open(path) else { return data };
    let mut bufreader = BufReader::new(file);
    let Ok(exifreader) = exif::Reader::new().read_from_container(&mut bufreader) else {
        return data;
    };

    for tag in [Tag::DateTimeOriginal, Tag::DateTime, Tag::DateTimeDigitized] {
        if let Some(field) = exifreader.get_field(tag, In::PRIMARY) {
            data.datetime = field.display_value().to_string();
            break;
        }
    }

    let lat_field = exifreader.get_field(Tag::GPSLatitude, In::PRIMARY);
    let lat_ref = exifreader.get_field(Tag::GPSLatitudeRef, In::PRIMARY);
    let lon_field = exifreader.get_field(Tag::GPSLongitude, In::PRIMARY);
    let lon_ref = exifreader.get_field(Tag::GPSLongitudeRef, In::PRIMARY);

    let mut lat = lat_field.and_then(dms_to_deg);
    let mut lon = lon_field.and_then(dms_to_deg);
    if let (Some(l), Some(r)) = (lat.as_mut(), lat_ref) {
        if r.display_value().to_string().starts_with('S') {
            *l = -*l;
        }
    }
    if let (Some(l), Some(r)) = (lon.as_mut(), lon_ref) {
        if r.display_value().to_string().starts_with('W') {
            *l = -*l;
        }
    }
    data.gps_lat = lat;
    data.gps_lon = lon;

    data.camera_make = exifreader
        .get_field(Tag::Make, In::PRIMARY)
        .map(|f| clean(&f.display_value().to_string()))
        .unwrap_or_default();
    data.camera_model = exifreader
        .get_field(Tag::Model, In::PRIMARY)
        .map(|f| clean(&f.display_value().to_string()))
        .unwrap_or_default();
    data.flash_fired = exifreader
        .get_field(Tag::Flash, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|v| v & 0x1 == 1); // low bit of the Flash tag = "flash fired"

    data
}

/// Heuristic: real camera/phone photos almost always carry a Make/Model
/// EXIF tag; screenshots, downloaded images and memes typically carry
/// neither. Not 100% reliable (a re-saved/edited photo can lose EXIF too),
/// but a useful cheap signal for filtering a phone photo dump.
pub fn is_screenshot_heuristic(data: &ExifData) -> bool {
    data.camera_make.is_empty() && data.camera_model.is_empty()
}
