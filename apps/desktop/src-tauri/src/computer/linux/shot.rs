//! Screenshots on Linux (S2): the portal returns one PNG of the whole layout, so the capture is
//! that PNG cropped to one output and downscaled like the other platforms'. Also what the
//! portal's answer means. Pure, so it is tested everywhere; `portal` does the D-Bus calls.

use super::toplevels::OutputInfo;
use crate::computer::coords::{fit_long_edge, Point, ScreenshotGeometry, MAX_LONG_EDGE};
use crate::computer::encode::{bgra_to_jpeg, JPEG_QUALITY};
use crate::computer::Capture;
use std::path::PathBuf;

/// How far the PNG's aspect may drift from the layout's before it is not trusted to match it.
const ASPECT_TOLERANCE: f64 = 0.02;

/// A region of the PNG, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The output's region of a PNG that covers the whole layout (the bounding box of the outputs),
/// `image` being the PNG's size in pixels.
pub fn crop_for(output: &OutputInfo, outputs: &[OutputInfo], image: (u32, u32)) -> Result<Crop, String> {
    let (left, top) = (outputs.iter().map(|o| o.rect.x).min(), outputs.iter().map(|o| o.rect.y).min());
    let right = outputs.iter().map(|o| o.rect.x + o.rect.width).max();
    let bottom = outputs.iter().map(|o| o.rect.y + o.rect.height).max();
    let (Some(left), Some(top), Some(right), Some(bottom)) = (left, top, right, bottom) else {
        return Err("no screen is known".into());
    };
    let (layout_w, layout_h) = (f64::from(right - left), f64::from(bottom - top));
    let (image_w, image_h) = (f64::from(image.0), f64::from(image.1));
    if layout_w <= 0.0 || layout_h <= 0.0 || image_w <= 0.0 || image_h <= 0.0 {
        return Err("the screenshot or the screen layout is empty".into());
    }
    let scale = image_w / layout_w;
    if (image_h / layout_h - scale).abs() > ASPECT_TOLERANCE * scale {
        return Err(format!(
            "the screenshot ({} × {} pixels) does not match the screen layout ({} × {} points)",
            image.0, image.1, layout_w, layout_h
        ));
    }
    let edge = |from: i32, origin: i32| (f64::from(from - origin) * scale).round().max(0.0) as u32;
    let (x, y) = (edge(output.rect.x, left).min(image.0 - 1), edge(output.rect.y, top).min(image.1 - 1));
    let width = ((f64::from(output.rect.width) * scale).round() as u32).clamp(1, image.0 - x);
    let height = ((f64::from(output.rect.height) * scale).round() as u32).clamp(1, image.1 - y);
    Ok(Crop { x, y, width, height })
}

/// The JPEG capture of `output` out of the portal's PNG: cropped, downscaled to the long edge
/// every screenshot has, and described by its geometry in global points.
pub fn capture_of(png: &[u8], output: &OutputInfo, outputs: &[OutputInfo]) -> Result<Capture, String> {
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| format!("cannot read the screenshot: {e}"))?
        .to_rgba8();
    let crop = crop_for(output, outputs, decoded.dimensions())?;
    let region = image::imageops::crop_imm(&decoded, crop.x, crop.y, crop.width, crop.height).to_image();
    capture_of_region(region, output)
}

/// The JPEG capture of `output` out of an image of exactly that output.
pub fn capture_of_region(region: image::RgbaImage, output: &OutputInfo) -> Result<Capture, String> {
    let (crop_w, crop_h) = region.dimensions();
    let (width, height) = fit_long_edge(crop_w, crop_h, MAX_LONG_EDGE);
    let scaled = if (width, height) == (crop_w, crop_h) {
        region
    } else {
        image::imageops::resize(&region, width, height, image::imageops::FilterType::Lanczos3)
    };
    let mut bgra = scaled.into_raw();
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let data = bgra_to_jpeg(&bgra, width, height, width as usize * 4, JPEG_QUALITY)?;
    let origin = Point { x: f64::from(output.rect.x), y: f64::from(output.rect.y) };
    let geometry = ScreenshotGeometry::for_display(origin, f64::from(output.rect.width), width, height);
    Ok(Capture { data, mime_type: "image/jpeg", geometry })
}

/// The object path the portal answers a request on, so the `Response` signal can be watched
/// before the call (xdg-desktop-portal, "Request" objects): the caller's unique bus name with
/// the `:` dropped and `.` turned into `_`, then the `handle_token`.
pub fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

fn percent_decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3).ok_or("a broken % escape in the screenshot path")?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| "a broken % escape in the screenshot path")?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "the screenshot path is not UTF-8".into())
}

/// The file a `file://` URI names.
pub fn uri_to_path(uri: &str) -> Result<PathBuf, String> {
    let rest = uri.strip_prefix("file://").ok_or_else(|| format!("the portal answered with `{uri}`, not a file"))?;
    // `file://localhost/path` and `file:///path`.
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return Err(format!("the portal answered with `{uri}`, not a local file"));
    }
    Ok(PathBuf::from(percent_decode(path)?))
}

/// What the portal's `Response(code, results)` means: the screenshot file, or why there is none.
pub fn response_file(code: u32, uri: Option<&str>) -> Result<PathBuf, String> {
    match (code, uri) {
        (0, Some(uri)) => uri_to_path(uri),
        (0, None) => Err("the screenshot portal answered without a file".into()),
        (1, _) => Err("the screenshot was cancelled".into()),
        (code, _) => Err(format!("the screenshot portal could not take the screenshot (response {code})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::linux::pointer::Rect;

    fn output(id: u32, x: i32, y: i32, width: i32, height: i32) -> OutputInfo {
        OutputInfo { id, name: format!("out-{id}"), rect: Rect { x, y, width, height } }
    }

    fn two_outputs() -> Vec<OutputInfo> {
        vec![output(1, 0, 0, 1920, 1080), output(2, 1920, 0, 1920, 1080)]
    }

    #[test]
    fn each_output_gets_its_own_region_of_the_layout_png() {
        let outputs = two_outputs();
        let size = (3840, 1080);
        assert_eq!(crop_for(&outputs[0], &outputs, size), Ok(Crop { x: 0, y: 0, width: 1920, height: 1080 }));
        assert_eq!(crop_for(&outputs[1], &outputs, size), Ok(Crop { x: 1920, y: 0, width: 1920, height: 1080 }));
    }

    #[test]
    fn the_crop_follows_the_png_scale_and_the_layouts_origin() {
        // A layout that starts at negative coordinates, captured at twice the points.
        let outputs = vec![output(1, -1920, -100, 1920, 1080), output(2, 0, 0, 2560, 1440)];
        let size = (4480 * 2, 1540 * 2);
        assert_eq!(crop_for(&outputs[0], &outputs, size), Ok(Crop { x: 0, y: 0, width: 3840, height: 2160 }));
        assert_eq!(crop_for(&outputs[1], &outputs, size), Ok(Crop { x: 3840, y: 200, width: 5120, height: 2880 }));
    }

    #[test]
    fn a_png_that_does_not_fit_the_layout_is_refused() {
        let outputs = two_outputs();
        assert!(crop_for(&outputs[0], &outputs, (3840, 2000)).unwrap_err().contains("does not match"));
        assert!(crop_for(&outputs[0], &[], (3840, 1080)).is_err());
        assert!(crop_for(&outputs[0], &outputs, (0, 0)).is_err());
    }

    #[test]
    fn the_capture_is_the_chosen_output_downscaled_with_its_geometry() {
        let outputs = two_outputs();
        let mut png = image::RgbImage::new(3840, 1080);
        for (x, _, pixel) in png.enumerate_pixels_mut() {
            *pixel = if x < 1920 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) };
        }
        let mut bytes = Vec::new();
        png.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();

        let capture = capture_of(&bytes, &outputs[1], &outputs).unwrap();
        assert_eq!(capture.mime_type, "image/jpeg");
        assert_eq!((capture.geometry.image_width, capture.geometry.image_height), (1280, 720));
        assert_eq!(capture.geometry.origin, Point { x: 1920.0, y: 0.0 });
        assert!((capture.geometry.scale - 1280.0 / 1920.0).abs() < 1e-9);
        let decoded = image::load_from_memory_with_format(&capture.data, image::ImageFormat::Jpeg).unwrap().to_rgb8();
        let [r, g, b] = decoded.get_pixel(640, 360).0;
        assert!(b > 200 && r < 60 && g < 60, "the right output is blue: {r} {g} {b}");
        let first = capture_of(&bytes, &outputs[0], &outputs).unwrap();
        let [r, _, b] = image::load_from_memory_with_format(&first.data, image::ImageFormat::Jpeg).unwrap().to_rgb8().get_pixel(5, 5).0;
        assert!(r > 200 && b < 60, "the left output is red: {r} {b}");
        assert!(capture_of(b"not a png", &outputs[0], &outputs).is_err());
    }

    #[test]
    fn a_small_output_is_not_upscaled() {
        let outputs = vec![output(1, 0, 0, 800, 600)];
        let mut bytes = Vec::new();
        image::RgbImage::new(800, 600).write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        let capture = capture_of(&bytes, &outputs[0], &outputs).unwrap();
        assert_eq!((capture.geometry.image_width, capture.geometry.image_height, capture.geometry.scale), (800, 600, 1.0));
    }

    #[test]
    fn request_paths_follow_the_portal_naming() {
        assert_eq!(
            request_path(":1.234", "gentle_dot_7"),
            "/org/freedesktop/portal/desktop/request/1_234/gentle_dot_7"
        );
    }

    #[test]
    fn portal_answers_become_a_file_or_a_reason() {
        let ok = response_file(0, Some("file:///tmp/screenshot-1%202.png"));
        assert_eq!(ok, Ok(PathBuf::from("/tmp/screenshot-1 2.png")));
        assert_eq!(response_file(0, Some("file://localhost/tmp/a.png")), Ok(PathBuf::from("/tmp/a.png")));
        assert!(response_file(0, None).is_err());
        assert!(response_file(1, None).unwrap_err().contains("cancelled"));
        assert!(response_file(2, Some("file:///tmp/a.png")).unwrap_err().contains("response 2"));
        for bad in ["https://example.com/a.png", "file://host/tmp/a.png", "file:///tmp/%zz.png", "file:///tmp/%2"] {
            assert!(response_file(0, Some(bad)).is_err(), "{bad}");
        }
    }
}
