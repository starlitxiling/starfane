//! Image upload storage and validation (`POST /api/uploads/image` & `GET /api/uploads/{id}`).
//!
//! Zero external crates: includes a strict std-only RFC-4648 base64 decoder, magic-byte
//! verification (PNG / JPEG / WebP / GIF), 10 MiB hard cap, and `0600` persistence inside
//! `<state_dir>/inputs/img-<id>.<ext>`.

use baton_proto::{id, ApiError};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Hard decoded image byte limit (10 MiB).
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// HTTP body limit for base64 JSON upload payload (15 MiB).
pub const MAX_UPLOAD_BODY_BYTES: usize = 15 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UploadedImage {
    pub id: String,
    pub path: String,
    pub filename: String,
    pub mime: String,
    pub size_bytes: usize,
    pub preview_url: String,
}

/// Detect image format strictly from leading magic bytes.
/// Returns `(mime, extension)`.
pub fn detect_image_magic(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
        return Some(("image/png", "png"));
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some(("image/jpeg", "jpg"));
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some(("image/gif", "gif"));
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(("image/webp", "webp"));
    }
    None
}

/// Decode standard (or URL-safe) base64, tolerating ASCII whitespace and optional `data:...;base64,` prefix.
pub fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let trimmed = input.trim();
    let payload = if let Some(rest) = trimmed.strip_prefix("data:") {
        match rest.split_once(";base64,") {
            Some((_, b64)) => b64,
            None => return Err("unsupported data URI (missing ;base64,)".into()),
        }
    } else {
        trimmed
    };

    let mut clean: Vec<u8> = Vec::with_capacity(payload.len());
    for b in payload.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        clean.push(b);
    }
    if clean.is_empty() {
        return Err("base64 payload is empty".into());
    }
    if clean.len() % 4 != 0 {
        return Err("invalid base64 length".into());
    }

    let mut out = Vec::with_capacity((clean.len() / 4) * 3);
    for (idx, chunk) in clean.chunks_exact(4).enumerate() {
        let last_chunk = (idx + 1) * 4 == clean.len();
        let a = b64_val(chunk[0]).ok_or("invalid base64 character")?;
        let b = b64_val(chunk[1]).ok_or("invalid base64 character")?;
        let pad_c = chunk[2] == b'=';
        let pad_d = chunk[3] == b'=';
        if (pad_c || pad_d) && !last_chunk {
            return Err("unexpected base64 padding before end".into());
        }
        if pad_c && !pad_d {
            return Err("invalid base64 padding".into());
        }
        let c = if pad_c {
            0
        } else {
            b64_val(chunk[2]).ok_or("invalid base64 character")?
        };
        let d = if pad_d {
            0
        } else {
            b64_val(chunk[3]).ok_or("invalid base64 character")?
        };

        let triple =
            (u32::from(a) << 18) | (u32::from(b) << 12) | (u32::from(c) << 6) | u32::from(d);
        out.push(((triple >> 16) & 0xFF) as u8);
        if !pad_c {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if !pad_d {
            out.push((triple & 0xFF) as u8);
        }
    }
    Ok(out)
}

fn b64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Validate, decode, and persist an uploaded image into `<state_dir>/inputs/img-<id>.<ext>` (0600).
pub fn save_uploaded_image(
    state_dir: &Path,
    filename: Option<&str>,
    claimed_mime: Option<&str>,
    data_base64: &str,
) -> Result<UploadedImage, ApiError> {
    let bytes = decode_base64(data_base64)
        .map_err(|e| ApiError::bad_request(format!("invalid image base64: {e}")))?;
    if bytes.is_empty() {
        return Err(ApiError::bad_request("image is empty"));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ApiError::bad_request(format!(
            "image too large ({} bytes, max {} bytes)",
            bytes.len(),
            MAX_IMAGE_BYTES
        )));
    }

    let (mime, ext) = detect_image_magic(&bytes).ok_or_else(|| {
        ApiError::bad_request("unsupported image format (expected PNG, JPEG, WebP, or GIF)")
    })?;
    if let Some(claimed) = claimed_mime.map(str::trim).filter(|s| !s.is_empty()) {
        let c = claimed.to_ascii_lowercase();
        let compatible = c == mime
            || (mime == "image/jpeg" && c == "image/jpg")
            || c == "application/octet-stream";
        if !compatible {
            return Err(ApiError::bad_request(format!(
                "image mime mismatch: declared {claimed}, detected {mime}"
            )));
        }
    }

    let raw_id = id::new_id("I").to_ascii_lowercase();
    let img_id = format!("img-{raw_id}");
    let path = state_dir.join("inputs").join(format!("{img_id}.{ext}"));
    crate::store::write_private(&path, &bytes)
        .map_err(|e| ApiError::internal(format!("save uploaded image {}: {e}", path.display())))?;

    let clean_name = filename
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| Path::new(s).file_name().and_then(|n| n.to_str()))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{img_id}.{ext}"));

    Ok(UploadedImage {
        preview_url: format!("/api/uploads/{img_id}"),
        id: img_id,
        path: path.to_string_lossy().into_owned(),
        filename: clean_name,
        mime: mime.to_string(),
        size_bytes: bytes.len(),
    })
}

/// Strictly validate an upload ID (prevent path traversal).
pub fn is_safe_upload_id(id: &str) -> bool {
    id.starts_with("img-")
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Load a previously uploaded image by ID for `GET /api/uploads/{id}`.
pub fn load_uploaded_image(
    state_dir: &Path,
    img_id: &str,
) -> Result<(Vec<u8>, &'static str), ApiError> {
    if !is_safe_upload_id(img_id) {
        return Err(ApiError::bad_request("invalid upload id"));
    }
    let inputs = state_dir.join("inputs");
    for (ext, mime) in [
        ("png", "image/png"),
        ("jpg", "image/jpeg"),
        ("webp", "image/webp"),
        ("gif", "image/gif"),
    ] {
        let p = inputs.join(format!("{img_id}.{ext}"));
        if p.is_file() {
            let bytes = std::fs::read(&p)
                .map_err(|e| ApiError::internal(format!("read upload {}: {e}", p.display())))?;
            return Ok((bytes, mime));
        }
    }
    Err(ApiError::not_found(format!("upload {img_id} not found")))
}

/// Verify that a dispatch image attachment path resolves strictly inside `<state_dir>/inputs/`.
pub fn validate_uploaded_image_path(state_dir: &Path, raw_path: &str) -> Result<PathBuf, ApiError> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("image attachment path is empty"));
    }
    let inputs_dir = state_dir.join("inputs");
    let canon_inputs = std::fs::canonicalize(&inputs_dir).map_err(|_| {
        ApiError::bad_request("image attachment path is not in baton inputs directory")
    })?;
    let canon_file = std::fs::canonicalize(trimmed)
        .map_err(|_| ApiError::bad_request(format!("image attachment not found: {trimmed}")))?;
    if !canon_file.starts_with(&canon_inputs) || !canon_file.is_file() {
        return Err(ApiError::bad_request(
            "image attachment path must stay inside baton inputs directory",
        ));
    }
    let ext = canon_file
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
        return Err(ApiError::bad_request(
            "image attachment must have a valid image extension",
        ));
    }
    Ok(canon_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal valid 1x1 PNG header + bytes for magic check.
    const SAMPLE_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[test]
    fn base64_and_magic_bytes_roundtrip_and_reject_invalid() {
        let raw = decode_base64(SAMPLE_PNG_B64).unwrap();
        assert_eq!(detect_image_magic(&raw), Some(("image/png", "png")));

        // Data URI prefix works too.
        let raw2 = decode_base64(&format!("data:image/png;base64,{SAMPLE_PNG_B64}")).unwrap();
        assert_eq!(raw, raw2);

        // Non-image bytes rejected.
        assert_eq!(detect_image_magic(b"not an image"), None);
        assert!(decode_base64("abc").is_err());
        assert!(decode_base64("!!!!").is_err());
    }

    #[test]
    fn save_load_and_validate_image_path_blocks_traversal() {
        let dir = std::env::temp_dir().join(format!("baton-upload-test-{}", id::new_id("T")));
        std::fs::create_dir_all(&dir).unwrap();

        let uploaded =
            save_uploaded_image(&dir, Some("screen.png"), Some("image/png"), SAMPLE_PNG_B64)
                .unwrap();
        assert!(uploaded.id.starts_with("img-"));
        assert_eq!(uploaded.mime, "image/png");
        assert_eq!(
            uploaded.preview_url,
            format!("/api/uploads/{}", uploaded.id)
        );

        let (bytes, mime) = load_uploaded_image(&dir, &uploaded.id).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(bytes.len(), uploaded.size_bytes);

        // Traversal attempts blocked on both GET id and dispatch path validator.
        assert!(load_uploaded_image(&dir, "../state.json").is_err());
        assert!(load_uploaded_image(&dir, "img-a/../b").is_err());
        assert!(validate_uploaded_image_path(&dir, "/etc/hosts").is_err());
        assert!(validate_uploaded_image_path(&dir, &uploaded.path).is_ok());

        // Fake text payload disguised as image/png rejected by magic bytes.
        let fake_b64 = "aGVsbG8gd29ybGQ="; // "hello world"
        assert!(save_uploaded_image(&dir, Some("x.png"), Some("image/png"), fake_b64).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
