use std::fmt;

use base64::Engine;
use base64::engine::general_purpose;
use image::{DynamicImage, ImageFormat};

pub const MAX_DIM: u32 = 1400;

#[derive(Debug)]
pub enum ImgError {
    Base64(base64::DecodeError),
    Image(image::ImageError),
    Malformed,
}

impl fmt::Display for ImgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImgError::Base64(e) => write!(f, "base64 decode error: {e}"),
            ImgError::Image(e) => write!(f, "image error: {e}"),
            ImgError::Malformed => write!(f, "malformed data URL: no comma"),
        }
    }
}

impl std::error::Error for ImgError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ImgError::Base64(e) => Some(e),
            ImgError::Image(e) => Some(e),
            ImgError::Malformed => None,
        }
    }
}

impl From<base64::DecodeError> for ImgError {
    fn from(e: base64::DecodeError) -> Self {
        ImgError::Base64(e)
    }
}

impl From<image::ImageError> for ImgError {
    fn from(e: image::ImageError) -> Self {
        ImgError::Image(e)
    }
}

pub fn fix_url(url: &str) -> Result<Option<String>, ImgError> {
    if !url.starts_with("data:image/") {
        return Ok(None);
    }

    let Some((header, b64_payload)) = url.split_once(',') else {
        return Err(ImgError::Malformed);
    };

    let mime = header
        .strip_prefix("data:")
        .unwrap_or(header)
        .split(';')
        .next()
        .unwrap_or("");

    if mime == "image/png" || mime == "image/jpeg" {
        return Ok(None);
    }

    let bytes = general_purpose::STANDARD.decode(b64_payload)?;
    let img = image::load_from_memory(&bytes)?.to_rgb8();

    let (w, h) = img.dimensions();
    let (new_w, new_h) = if w > MAX_DIM || h > MAX_DIM {
        let scale = MAX_DIM as f64 / f64::max(w as f64, h as f64);
        let nw = (w as f64 * scale).round() as u32;
        let nh = (h as f64 * scale).round() as u32;
        (nw, nh)
    } else {
        (w, h)
    };

    let resized = if (new_w, new_h) != (w, h) {
        DynamicImage::ImageRgb8(image::imageops::resize(
            &img,
            new_w,
            new_h,
            image::imageops::FilterType::Lanczos3,
        ))
    } else {
        DynamicImage::ImageRgb8(img)
    };

    let mut buf = Vec::new();
    resized.write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)?;

    let encoded = general_purpose::STANDARD.encode(&buf);
    Ok(Some(format!("data:image/png;base64,{encoded}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose;
    use image::GenericImageView;

    fn make_rgb(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::WebP)
            .unwrap();
        buf
    }

    #[test]
    fn not_an_image() {
        assert!(fix_url("https://example.com/a.png").is_ok_and(|v| v.is_none()));
    }

    #[test]
    fn png_passthrough() {
        let b64 = general_purpose::STANDARD.encode(&[0u8; 4]);
        let url = format!("data:image/png;base64,{b64}");
        assert!(fix_url(&url).is_ok_and(|v| v.is_none()));
    }

    #[test]
    fn webp_large_is_converted() {
        let raw = make_rgb(2000, 1000);
        let b64 = general_purpose::STANDARD.encode(&raw);
        let url = format!("data:image/webp;base64,{b64}");
        let result = fix_url(&url).expect("conversion should succeed");
        let Some(out) = result else { panic!("expected Some" ) };
        assert!(out.starts_with("data:image/png;base64,"));
        let payload = out["data:image/png;base64,".len()..].to_string();
        let bytes = general_purpose::STANDARD
            .decode(&payload)
            .expect("valid base64");
        let decoded = image::load_from_memory(&bytes).expect("valid PNG");
        assert_eq!(decoded.dimensions(), (1400, 700));
    }

    #[test]
    fn webp_small_not_upscaled() {
        let raw = make_rgb(300, 200);
        let b64 = general_purpose::STANDARD.encode(&raw);
        let url = format!("data:image/webp;base64,{b64}");
        let result = fix_url(&url).expect("conversion should succeed");
        let Some(out) = result else { panic!("expected Some" ) };
        assert!(out.starts_with("data:image/png;base64,"));
        let payload = out["data:image/png;base64,".len()..].to_string();
        let bytes = general_purpose::STANDARD
            .decode(&payload)
            .expect("valid base64");
        let decoded = image::load_from_memory(&bytes).expect("valid PNG");
        assert_eq!(decoded.dimensions(), (300, 200));
    }

    #[test]
    fn broken_base64() {
        let result = fix_url("data:image/webp;base64,@@@notbase64");
        assert!(matches!(result, Err(ImgError::Base64(_))));
    }

    #[test]
    fn no_comma() {
        let result = fix_url("data:image/webp");
        assert!(matches!(result, Err(ImgError::Malformed)));
    }
}
