use exif::{In, Tag, Value};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub fn read_datetime(path: &Path) -> String {
    let Ok(file) = File::open(path) else { return String::new(); };
    let mut bufreader = BufReader::new(file);
    let Ok(exifreader) = exif::Reader::new().read_from_container(&mut bufreader) else {
        return String::new();
    };
    for tag in [Tag::DateTimeOriginal, Tag::DateTime, Tag::DateTimeDigitized] {
        if let Some(field) = exifreader.get_field(tag, In::PRIMARY) {
            return field.display_value().to_string();
        }
    }
    String::new()
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

pub fn read_gps(path: &Path) -> (Option<f64>, Option<f64>) {
    let Ok(file) = File::open(path) else { return (None, None); };
    let mut bufreader = BufReader::new(file);
    let Ok(exifreader) = exif::Reader::new().read_from_container(&mut bufreader) else {
        return (None, None);
    };

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
    (lat, lon)
}
