//! Lazy-loading HEIC/HEIF decoding via the system libheif C library.
//!
//! libheif is loaded at runtime (dlopen / LoadLibrary) instead of being
//! linked, so cs-imageindex runs on machines without libheif and simply
//! skips HEIC/HEIF files there (see README "Supported formats"). The
//! `heic` cargo feature (default on) gates this module; there is no
//! build-time system dependency.
//!
//! Only a tiny, stable subset of the libheif C API is used
//! (heif_context_* / heif_image_handle_* / heif_decode_image /
//! heif_image_get_plane_readonly), all available since libheif 1.0.

use image::DynamicImage;
use libloading::{Library, Symbol};
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;

// opaque libheif types
#[repr(C)]
struct HeifContext {
    _priv: [u8; 0],
}
#[repr(C)]
struct HeifImageHandle {
    _priv: [u8; 0],
}
#[repr(C)]
struct HeifImage {
    _priv: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HeifError {
    code: c_int,
    subcode: c_int,
    message: *const c_char,
}

// enum values from <libheif.h>
const HEIF_COLORSPACE_RGB: c_int = 0;
const HEIF_CHROMA_INTERLEAVED_RGB: c_int = 10;
const HEIF_CHANNEL_INTERLEAVED: c_int = 10;

type SymContextAlloc = unsafe extern "C" fn() -> *mut HeifContext;
type SymContextFree = unsafe extern "C" fn(*mut HeifContext);
type SymReadFromFile =
    unsafe extern "C" fn(*mut HeifContext, *const c_char, *const c_void) -> HeifError;
type SymGetPrimary =
    unsafe extern "C" fn(*mut HeifContext, *mut *mut HeifImageHandle) -> HeifError;
type SymHandleRelease = unsafe extern "C" fn(*mut HeifImageHandle);
type SymDecode = unsafe extern "C" fn(
    *const HeifImageHandle,
    *mut *mut HeifImage,
    c_int,
    c_int,
    *const c_void,
) -> HeifError;
type SymImageRelease = unsafe extern "C" fn(*mut HeifImage);
type SymImageWidth = unsafe extern "C" fn(*const HeifImage, c_int) -> c_int;
type SymImageHeight = unsafe extern "C" fn(*const HeifImage, c_int) -> c_int;
type SymPlane = unsafe extern "C" fn(*const HeifImage, c_int, *mut c_int) -> *const u8;

/// Candidate library names, most specific first.
fn libheif_library_names() -> &'static [&'static str] {
    if cfg!(target_os = "illumos") {
        // ooce installs under /opt/ooce, which is not on the default dlopen path
        &["/opt/ooce/lib/amd64/libheif.so.1", "libheif.so.1", "libheif.so"]
    } else if cfg!(target_os = "macos") {
        &["libheif.dylib"]
    } else if cfg!(target_os = "windows") {
        &["libheif.dll", "libheif-1.dll"]
    } else {
        &["libheif.so.1", "libheif.so"]
    }
}

fn load_libheif() -> Option<Library> {
    libheif_library_names()
        .iter()
        .find_map(|n| unsafe { Library::new(n) }.ok())
}

fn err_text(e: &HeifError) -> String {
    if e.message.is_null() {
        format!("libheif error code {}", e.code)
    } else {
        unsafe { std::ffi::CStr::from_ptr(e.message).to_string_lossy().into_owned() }
    }
}

unsafe fn decode_with_ctx(
    lib: &Library,
    ctx: *mut HeifContext,
    cpath: &CString,
) -> Result<DynamicImage, String> {
    let read: Symbol<SymReadFromFile> = lib
        .get(b"heif_context_read_from_file\0")
        .map_err(|e| format!("HEIC: {e}"))?;
    let get_primary: Symbol<SymGetPrimary> = lib
        .get(b"heif_context_get_primary_image_handle\0")
        .map_err(|e| format!("HEIC: {e}"))?;
    let handle_release: Symbol<SymHandleRelease> = lib
        .get(b"heif_image_handle_release\0")
        .map_err(|e| format!("HEIC: {e}"))?;
    let decode: Symbol<SymDecode> = lib
        .get(b"heif_decode_image\0")
        .map_err(|e| format!("HEIC: {e}"))?;
    let image_release: Symbol<SymImageRelease> = lib
        .get(b"heif_image_release\0")
        .map_err(|e| format!("HEIC: {e}"))?;
    let image_width: Symbol<SymImageWidth> = lib
        .get(b"heif_image_get_width\0")
        .map_err(|e| format!("HEIC: {e}"))?;
    let image_height: Symbol<SymImageHeight> = lib
        .get(b"heif_image_get_height\0")
        .map_err(|e| format!("HEIC: {e}"))?;
    let plane: Symbol<SymPlane> = lib
        .get(b"heif_image_get_plane_readonly\0")
        .map_err(|e| format!("HEIC: {e}"))?;

    let err = read(ctx, cpath.as_ptr(), std::ptr::null());
    if err.code != 0 {
        return Err(format!("HEIC open: {}", err_text(&err)));
    }

    let mut handle: *mut HeifImageHandle = std::ptr::null_mut();
    let err = get_primary(ctx, &mut handle);
    if err.code != 0 || handle.is_null() {
        return Err(format!("HEIC handle: {}", err_text(&err)));
    }

    // decode() applies rotation/mirroring/cropping baked into the HEIF file,
    // the same role EXIF-orientation handling plays for JPEG.
    let mut img: *mut HeifImage = std::ptr::null_mut();
    let err = decode(
        handle,
        &mut img,
        HEIF_COLORSPACE_RGB,
        HEIF_CHROMA_INTERLEAVED_RGB,
        std::ptr::null(),
    );
    // capture error fields BEFORE any further libheif call (error messages may
    // point into per-context buffers that later calls can overwrite)
    let (ec, esc, em) = (err.code, err.subcode, err_text(&err));
    handle_release(handle);
    if ec != 0 || img.is_null() {
        eprintln!("[heic-debug] decode err code={ec} subcode={esc} img_null={} msg={em}", img.is_null());
        return Err(format!("HEIC decode: {em}"));
    }

    let iw = image_width(img, HEIF_CHANNEL_INTERLEAVED);
    let ih = image_height(img, HEIF_CHANNEL_INTERLEAVED);
    let mut stride: c_int = 0;
    let data = plane(img, HEIF_CHANNEL_INTERLEAVED, &mut stride);
    if data.is_null() || iw <= 0 || ih <= 0 || stride <= 0 {
        image_release(img);
        return Err("HEIC: no interleaved RGB plane".to_string());
    }

    let row_len = iw as usize * 3; // interleaved RGB
    let mut buf = Vec::with_capacity(ih as usize * row_len);
    for y in 0..ih as usize {
        let row = data.add(y * stride as usize);
        buf.extend_from_slice(std::slice::from_raw_parts(row, row_len));
    }
    image_release(img);

    image::RgbImage::from_raw(iw as u32, ih as u32, buf)
        .map(DynamicImage::ImageRgb8)
        .ok_or_else(|| "HEIC decode: pixel buffer/size mismatch".to_string())
}

/// Decode a HEIC/HEIF photo into an RGB image. Returns an error (callers skip
/// the file) if the system libheif is missing or the file cannot be decoded.
pub fn open_heic(path: &Path) -> Result<DynamicImage, String> {
    let lib = load_libheif().ok_or_else(|| {
        "HEIC: system libheif not available at runtime (dlopen failed); install libheif to enable HEIC/HEIF"
            .to_string()
    })?;

    let path_str = path.to_str().ok_or("HEIC: non-UTF-8 path")?;
    let cpath = CString::new(path_str).map_err(|_| "HEIC: path contains NUL byte")?;

    unsafe {
        let alloc: Symbol<SymContextAlloc> = lib.get(b"heif_context_alloc\0").map_err(|e| e.to_string())?;
        let ctx = alloc();
        if ctx.is_null() {
            return Err("HEIC: heif_context_alloc failed".to_string());
        }
        let free: Symbol<SymContextFree> = lib.get(b"heif_context_free\0").map_err(|e| e.to_string())?;
        let result = decode_with_ctx(&lib, ctx, &cpath);
        free(ctx);
        result
    }
}

