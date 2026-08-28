// Cheap, dependency-free image-quality signals computed directly off pixels
// already in memory -- no extra crates, no network, no ONNX model. Useful
// for filtering a large photo dump: blur_score to drop out-of-focus shots,
// perceptual_hash to spot near-duplicate/burst-mode shots.

use image::DynamicImage;

/// Laplacian-variance blur estimate: a sharp photo has lots of high-
/// frequency edge content, so a Laplacian filter's response has high
/// variance; a blurry one doesn't. This is a RELATIVE signal (higher =
/// sharper within this tool's own scale) -- it is not calibrated against
/// OpenCV's or any other implementation's absolute threshold, since the
/// downscale size and exact kernel application differ.
pub fn blur_score(img: &DynamicImage) -> f32 {
    // Downscale first: blur detection doesn't need full resolution, and
    // working on a fixed small size keeps the score comparable across
    // photos of very different original resolutions.
    let small = img.resize(400, 400, image::imageops::FilterType::Triangle);
    let gray = small.to_luma8();
    let (w, h) = gray.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }
    let mut values: Vec<f32> = Vec::with_capacity((w as usize - 2) * (h as usize - 2));
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let center = gray.get_pixel(x, y)[0] as f32;
            let up = gray.get_pixel(x, y - 1)[0] as f32;
            let down = gray.get_pixel(x, y + 1)[0] as f32;
            let left = gray.get_pixel(x - 1, y)[0] as f32;
            let right = gray.get_pixel(x + 1, y)[0] as f32;
            // Standard discrete Laplacian kernel [[0,1,0],[1,-4,1],[0,1,0]]
            values.push(up + down + left + right - 4.0 * center);
        }
    }
    let n = values.len() as f32;
    let mean: f32 = values.iter().sum::<f32>() / n;
    values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n
}

/// 64-bit average hash (aHash): resize to 8x8 grayscale, threshold each
/// pixel against the mean, pack the result into a u64. Two photos of the
/// same shot (burst mode, near-identical framing) typically end up with a
/// small Hamming distance between their hashes (XOR the two u64s, count set
/// bits) -- a distance under ~5-10 is a common rule of thumb for "likely
/// the same shot", though the right cutoff depends on the photo set.
pub fn perceptual_hash(img: &DynamicImage) -> u64 {
    let small = img
        .resize_exact(8, 8, image::imageops::FilterType::Triangle)
        .to_luma8();
    let pixels: Vec<u8> = small.pixels().map(|p| p[0]).collect();
    let mean = pixels.iter().map(|&p| p as u32).sum::<u32>() as f32 / pixels.len() as f32;
    let mut hash: u64 = 0;
    for (i, &p) in pixels.iter().enumerate() {
        if p as f32 >= mean {
            hash |= 1 << i;
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;

    #[test]
    fn flat_image_has_near_zero_blur_score() {
        // A perfectly flat image has zero edge content -- blur_score should
        // be (very close to) zero regardless of which flat color is used.
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(50, 50, image::Rgb([128, 128, 128])));
        let score = blur_score(&img);
        assert!(score < 0.001, "expected ~0 for a flat image, got {score}");
    }

    #[test]
    fn checkerboard_has_higher_blur_score_than_flat() {
        let flat = DynamicImage::ImageRgb8(RgbImage::from_pixel(50, 50, image::Rgb([128, 128, 128])));
        let mut checker = RgbImage::new(50, 50);
        for (x, y, px) in checker.enumerate_pixels_mut() {
            *px = if (x + y) % 2 == 0 { image::Rgb([0, 0, 0]) } else { image::Rgb([255, 255, 255]) };
        }
        let checker = DynamicImage::ImageRgb8(checker);
        assert!(blur_score(&checker) > blur_score(&flat));
    }

    #[test]
    fn identical_images_produce_identical_hash() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 64, image::Rgb([10, 200, 30])));
        assert_eq!(perceptual_hash(&img), perceptual_hash(&img.clone()));
    }

    #[test]
    fn very_different_images_produce_different_hashes() {
        let dark = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 64, image::Rgb([0, 0, 0])));
        let mut half = RgbImage::new(64, 64);
        for (x, _y, px) in half.enumerate_pixels_mut() {
            *px = if x < 32 { image::Rgb([0, 0, 0]) } else { image::Rgb([255, 255, 255]) };
        }
        let half = DynamicImage::ImageRgb8(half);
        assert_ne!(perceptual_hash(&dark), perceptual_hash(&half));
    }
}
