//! Shared image normalization, thumbnailing, and disk-pruning helpers.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageFormat};
use zune_core::{colorspace::ColorSpace, options::DecoderOptions};
use zune_jpeg::JpegDecoder;

const PIPELINE_VERSION: &str = "img-v1";
const DETAIL_PREVIEW_VERSION: &str = "detail-v2";
const DEFAULT_LIST_IMAGE_MAX_EDGE_PX: u32 = 320;
const DEFAULT_COVER_DISK_BUDGET_BYTES: u64 = 512u64 * 1024u64 * 1024u64;
const DEFAULT_ARTIST_DISK_BUDGET_BYTES: u64 = 256u64 * 1024u64 * 1024u64;

static LIST_IMAGE_MAX_EDGE_PX: AtomicU32 = AtomicU32::new(DEFAULT_LIST_IMAGE_MAX_EDGE_PX);
static COVER_DISK_BUDGET_BYTES: AtomicU64 = AtomicU64::new(DEFAULT_COVER_DISK_BUDGET_BYTES);
static ARTIST_DISK_BUDGET_BYTES: AtomicU64 = AtomicU64::new(DEFAULT_ARTIST_DISK_BUDGET_BYTES);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagedImageKind {
    CoverArt,
    ArtistImage,
}

pub fn runtime_cover_disk_budget_bytes() -> u64 {
    COVER_DISK_BUDGET_BYTES.load(Ordering::Relaxed)
}

pub fn runtime_artist_disk_budget_bytes() -> u64 {
    ARTIST_DISK_BUDGET_BYTES.load(Ordering::Relaxed)
}

pub fn configure_runtime_limits(
    list_image_max_edge_px: u32,
    cover_cache_max_size_mb: u32,
    artist_cache_max_size_mb: u32,
) {
    LIST_IMAGE_MAX_EDGE_PX.store(list_image_max_edge_px.max(1), Ordering::Relaxed);
    COVER_DISK_BUDGET_BYTES.store(mb_to_bytes(cover_cache_max_size_mb), Ordering::Relaxed);
    ARTIST_DISK_BUDGET_BYTES.store(mb_to_bytes(artist_cache_max_size_mb), Ordering::Relaxed);
}

pub fn mb_to_bytes(value_mb: u32) -> u64 {
    u64::from(value_mb.max(1)) * 1024u64 * 1024u64
}

fn roqtune_cache_root() -> Option<PathBuf> {
    dirs::cache_dir().map(|path| path.join("roqtune"))
}

pub fn kind_cache_root(kind: ManagedImageKind) -> Option<PathBuf> {
    match kind {
        ManagedImageKind::CoverArt => roqtune_cache_root().map(|path| path.join("covers")),
        ManagedImageKind::ArtistImage => {
            roqtune_cache_root().map(|path| path.join("library_enrichment"))
        }
    }
}

pub fn cover_originals_dir() -> Option<PathBuf> {
    kind_cache_root(ManagedImageKind::CoverArt).map(|path| path.join("original"))
}

pub fn cover_thumbs_dir(max_edge_px: u32) -> Option<PathBuf> {
    kind_cache_root(ManagedImageKind::CoverArt)
        .map(|path| path.join("thumbs").join(max_edge_px.to_string()))
}

pub fn artist_originals_dir() -> Option<PathBuf> {
    kind_cache_root(ManagedImageKind::ArtistImage).map(|path| path.join("images"))
}

pub fn artist_thumbs_dir(max_edge_px: u32) -> Option<PathBuf> {
    kind_cache_root(ManagedImageKind::ArtistImage)
        .map(|path| path.join("thumbs").join(max_edge_px.to_string()))
}

pub fn cover_detail_previews_dir(max_edge_px: u32) -> Option<PathBuf> {
    kind_cache_root(ManagedImageKind::CoverArt)
        .map(|path| path.join("detail").join(max_edge_px.to_string()))
}

pub fn artist_detail_previews_dir(max_edge_px: u32) -> Option<PathBuf> {
    kind_cache_root(ManagedImageKind::ArtistImage)
        .map(|path| path.join("detail").join(max_edge_px.to_string()))
}

fn originals_dir(kind: ManagedImageKind) -> Option<PathBuf> {
    match kind {
        ManagedImageKind::CoverArt => cover_originals_dir(),
        ManagedImageKind::ArtistImage => artist_originals_dir(),
    }
}

fn thumbs_dir(kind: ManagedImageKind, max_edge_px: u32) -> Option<PathBuf> {
    match kind {
        ManagedImageKind::CoverArt => cover_thumbs_dir(max_edge_px),
        ManagedImageKind::ArtistImage => artist_thumbs_dir(max_edge_px),
    }
}

fn detail_previews_dir(kind: ManagedImageKind, max_edge_px: u32) -> Option<PathBuf> {
    match kind {
        ManagedImageKind::CoverArt => cover_detail_previews_dir(max_edge_px),
        ManagedImageKind::ArtistImage => artist_detail_previews_dir(max_edge_px),
    }
}

fn ensure_parent_dir(path: &Path) -> Option<()> {
    let parent = path.parent()?;
    if !parent.exists() {
        fs::create_dir_all(parent).ok()?;
    }
    Some(())
}

fn image_is_decodable(path: &Path) -> bool {
    image_dimensions_with_fallback(path).is_some()
}

fn hash_string(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn source_fingerprint(path: &Path) -> String {
    let metadata = fs::metadata(path).ok();
    let size = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
    let modified_secs = metadata
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!(
        "{PIPELINE_VERSION}|{}|{}|{}",
        path.to_string_lossy(),
        size,
        modified_secs
    )
}

pub fn cover_original_path_for_track(track_path: &Path) -> Option<PathBuf> {
    let key = format!(
        "{PIPELINE_VERSION}|source-key|{}",
        track_path.to_string_lossy()
    );
    let stem = hash_string(&key);
    Some(cover_originals_dir()?.join(format!("{stem}.png")))
}

pub fn normalize_and_cache_original_bytes(
    kind: ManagedImageKind,
    source_key: &str,
    bytes: &[u8],
) -> Option<PathBuf> {
    let decoded = decode_image_from_memory_with_fallback(bytes)?;
    let stem = hash_string(&format!("{PIPELINE_VERSION}|source-key|{source_key}"));
    let target_path = originals_dir(kind)?.join(format!("{stem}.png"));
    ensure_parent_dir(&target_path)?;

    if target_path.exists() && image_is_decodable(&target_path) {
        return Some(target_path);
    }

    let temp_path = target_path.with_extension("png.tmp");
    save_png_atomic(&decoded, &temp_path, &target_path)?;
    Some(target_path)
}

fn save_png_atomic(image: &DynamicImage, temp_path: &Path, target_path: &Path) -> Option<()> {
    if temp_path.exists() {
        let _ = fs::remove_file(temp_path);
    }
    image.save_with_format(temp_path, ImageFormat::Png).ok()?;
    fs::rename(temp_path, target_path).ok()?;
    Some(())
}

fn looks_like_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] == 0xd8
}

fn decode_jpeg_non_strict(bytes: &[u8]) -> Option<DynamicImage> {
    if !looks_like_jpeg(bytes) {
        return None;
    }

    let options = DecoderOptions::new_cmd()
        .set_strict_mode(false)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(bytes, options);
    let pixels = decoder.decode().ok()?;
    let (width, height) = decoder.dimensions()?;
    let image = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
    Some(DynamicImage::ImageRgba8(image))
}

fn decode_image_from_memory_with_fallback(bytes: &[u8]) -> Option<DynamicImage> {
    // Preserve broad support for PNG/WebP/GIF/BMP/etc via the primary decoder.
    // Use non-strict JPEG fallback only when the primary path fails.
    image::load_from_memory(bytes)
        .ok()
        .or_else(|| decode_jpeg_non_strict(bytes))
}

fn decode_image_from_path_with_fallback(path: &Path) -> Option<DynamicImage> {
    image::open(path).ok().or_else(|| {
        let bytes = fs::read(path).ok()?;
        decode_image_from_memory_with_fallback(&bytes)
    })
}

fn image_dimensions_with_fallback(path: &Path) -> Option<(u32, u32)> {
    image::image_dimensions(path)
        .ok()
        .or_else(|| decode_image_from_path_with_fallback(path).map(|decoded| decoded.dimensions()))
}

fn fit_to_max_edge(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (1, 1);
    }
    let clamped = max_edge.max(1);
    if width.max(height) <= clamped {
        return (width, height);
    }
    if width >= height {
        let scaled_height =
            ((u64::from(height) * u64::from(clamped)) + (u64::from(width) / 2)) / u64::from(width);
        (clamped, scaled_height.max(1) as u32)
    } else {
        let scaled_width =
            ((u64::from(width) * u64::from(clamped)) + (u64::from(height) / 2)) / u64::from(height);
        (scaled_width.max(1) as u32, clamped)
    }
}

fn resize_for_detail_display(
    decoded: DynamicImage,
    target_width: u32,
    target_height: u32,
) -> DynamicImage {
    let mut current = decoded;
    let mut current_dims = current.dimensions();
    let target_w = target_width.max(1);
    let target_h = target_height.max(1);

    // Multi-stage reduction reduces aliasing/moire on very high-resolution scans.
    while current_dims.0 > target_w.saturating_mul(2) || current_dims.1 > target_h.saturating_mul(2)
    {
        let next_w = (current_dims.0 / 2).max(target_w);
        let next_h = (current_dims.1 / 2).max(target_h);
        current = current.resize(next_w, next_h, FilterType::Triangle);
        current_dims = current.dimensions();
    }

    if current_dims.0 == target_w && current_dims.1 == target_h {
        return current;
    }

    let current_long_edge = current_dims.0.max(current_dims.1).max(1) as f32;
    let target_long_edge = target_w.max(target_h).max(1) as f32;
    let ratio = current_long_edge / target_long_edge;
    let antialias_source = if ratio > 1.8 {
        current.blur(0.6)
    } else {
        current
    };
    let final_filter = if ratio > 2.6 {
        FilterType::Gaussian
    } else {
        FilterType::CatmullRom
    };
    antialias_source.resize(target_w, target_h, final_filter)
}

fn thumbnail_stem(path: &Path, max_edge_px: u32) -> String {
    hash_string(&format!(
        "{PIPELINE_VERSION}|thumb|{}|{}",
        source_fingerprint(path),
        max_edge_px
    ))
}

fn detail_preview_stem(path: &Path, max_edge_px: u32) -> String {
    hash_string(&format!(
        "{PIPELINE_VERSION}|{DETAIL_PREVIEW_VERSION}|{}|{}",
        source_fingerprint(path),
        max_edge_px
    ))
}

pub fn list_thumbnail_path(
    kind: ManagedImageKind,
    source_path: &Path,
    max_edge_px: u32,
) -> Option<PathBuf> {
    let stem = thumbnail_stem(source_path, max_edge_px);
    Some(thumbs_dir(kind, max_edge_px)?.join(format!("{stem}.png")))
}

pub fn list_thumbnail_path_if_present(
    kind: ManagedImageKind,
    source_path: &Path,
    max_edge_px: u32,
) -> Option<PathBuf> {
    let candidate = list_thumbnail_path(kind, source_path, max_edge_px)?;
    if !candidate.exists() {
        return None;
    }
    if !image_is_decodable(&candidate) {
        let _ = fs::remove_file(&candidate);
        return None;
    }
    Some(candidate)
}

pub fn ensure_list_thumbnail(
    kind: ManagedImageKind,
    source_path: &Path,
    max_edge_px: u32,
) -> Option<PathBuf> {
    if let Some(existing) = list_thumbnail_path_if_present(kind, source_path, max_edge_px) {
        return Some(existing);
    }
    let decoded = decode_image_from_path_with_fallback(source_path)?;
    let (source_width, source_height) = decoded.dimensions();
    let (target_width, target_height) = fit_to_max_edge(source_width, source_height, max_edge_px);
    let resized = if target_width == source_width && target_height == source_height {
        decoded
    } else {
        decoded.resize(target_width, target_height, FilterType::Lanczos3)
    };
    let target_path = list_thumbnail_path(kind, source_path, max_edge_px)?;
    ensure_parent_dir(&target_path)?;
    let temp_path = target_path.with_extension("png.tmp");
    save_png_atomic(&resized, &temp_path, &target_path)?;
    Some(target_path)
}

pub fn detail_preview_path(
    kind: ManagedImageKind,
    source_path: &Path,
    max_edge_px: u32,
) -> Option<PathBuf> {
    let stem = detail_preview_stem(source_path, max_edge_px);
    Some(detail_previews_dir(kind, max_edge_px)?.join(format!("{stem}.png")))
}

pub fn detail_preview_path_if_present(
    kind: ManagedImageKind,
    source_path: &Path,
    max_edge_px: u32,
) -> Option<PathBuf> {
    let candidate = detail_preview_path(kind, source_path, max_edge_px)?;
    if !candidate.exists() {
        return None;
    }
    if !image_is_decodable(&candidate) {
        let _ = fs::remove_file(&candidate);
        return None;
    }
    Some(candidate)
}

pub fn ensure_detail_preview(
    kind: ManagedImageKind,
    source_path: &Path,
    max_edge_px: u32,
) -> Option<PathBuf> {
    ensure_detail_preview_with_threshold(kind, source_path, max_edge_px, max_edge_px)
}

pub fn ensure_detail_preview_with_threshold(
    kind: ManagedImageKind,
    source_path: &Path,
    max_edge_px: u32,
    convert_threshold_px: u32,
) -> Option<PathBuf> {
    let clamped_max_edge_px = max_edge_px.max(1);
    let clamped_convert_threshold_px = convert_threshold_px.max(clamped_max_edge_px);
    let strict_dimensions = image::image_dimensions(source_path).ok();
    if let Some((source_width, source_height)) = strict_dimensions {
        if source_width.max(source_height) <= clamped_convert_threshold_px {
            return Some(source_path.to_path_buf());
        }
    }

    if let Some(existing) = detail_preview_path_if_present(kind, source_path, clamped_max_edge_px) {
        return Some(existing);
    }

    let decoded = decode_image_from_path_with_fallback(source_path)?;
    let (source_width, source_height) = decoded.dimensions();
    let (target_width, target_height) =
        fit_to_max_edge(source_width, source_height, clamped_max_edge_px);
    let resized = resize_for_detail_display(decoded, target_width, target_height);
    let target_path = detail_preview_path(kind, source_path, clamped_max_edge_px)?;
    ensure_parent_dir(&target_path)?;
    let temp_path = target_path.with_extension("png.tmp");
    save_png_atomic(&resized, &temp_path, &target_path)?;
    let disk_budget = match kind {
        ManagedImageKind::CoverArt => runtime_cover_disk_budget_bytes(),
        ManagedImageKind::ArtistImage => runtime_artist_disk_budget_bytes(),
    };
    let _ = prune_kind_disk_cache(kind, disk_budget);
    Some(target_path)
}

pub fn decoded_rgba_bytes(path: &Path) -> Option<u64> {
    let (width, height) = image_dimensions_with_fallback(path)?;
    Some(u64::from(width) * u64::from(height) * 4u64)
}

fn list_files_recursive(root: &Path) -> Vec<(PathBuf, u64, u128)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or(0);
            files.push((path, metadata.len(), modified));
        }
    }
    files
}

pub fn prune_kind_disk_cache(kind: ManagedImageKind, max_size_bytes: u64) -> Vec<PathBuf> {
    let Some(root) = kind_cache_root(kind) else {
        return Vec::new();
    };
    let mut files = list_files_recursive(&root);
    if files.is_empty() {
        return Vec::new();
    }

    let mut total_bytes = files.iter().map(|(_, size, _)| *size).sum::<u64>();
    if total_bytes <= max_size_bytes {
        return Vec::new();
    }

    files.sort_by_key(|(_, _, modified)| *modified);
    let mut removed = Vec::new();
    for (path, size, _) in files {
        if total_bytes <= max_size_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total_bytes = total_bytes.saturating_sub(size);
            removed.push(path);
        }
    }
    removed
}

pub fn clear_kind_disk_cache(kind: ManagedImageKind) -> usize {
    let Some(root) = kind_cache_root(kind) else {
        return 0;
    };
    let mut deleted = 0usize;
    for (path, _, _) in list_files_recursive(&root) {
        if fs::remove_file(path).is_ok() {
            deleted = deleted.saturating_add(1);
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::{
        decode_image_from_memory_with_fallback, fit_to_max_edge, hash_string, mb_to_bytes,
        resize_for_detail_display,
    };
    use image::{
        codecs::jpeg::JpegEncoder, DynamicImage, GenericImageView, ImageBuffer, ImageFormat, Rgb,
        RgbImage, Rgba,
    };
    use std::io::Cursor;

    #[test]
    fn test_mb_to_bytes_never_returns_zero() {
        assert_eq!(mb_to_bytes(0), 1024u64 * 1024u64);
        assert_eq!(mb_to_bytes(5), 5u64 * 1024u64 * 1024u64);
    }

    #[test]
    fn test_hash_string_is_deterministic() {
        assert_eq!(hash_string("abc"), hash_string("abc"));
        assert_ne!(hash_string("abc"), hash_string("abcd"));
    }

    #[test]
    fn test_fit_to_max_edge_preserves_aspect_ratio() {
        assert_eq!(fit_to_max_edge(2000, 1000, 320), (320, 160));
        assert_eq!(fit_to_max_edge(1000, 2000, 320), (160, 320));
        assert_eq!(fit_to_max_edge(128, 64, 320), (128, 64));
    }

    #[test]
    fn test_resize_for_detail_display_matches_requested_dimensions() {
        let source = DynamicImage::ImageRgba8(ImageBuffer::from_fn(2200, 1800, |x, y| {
            if (x + y) % 2 == 0 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            }
        }));
        let resized = resize_for_detail_display(source, 384, 314);
        assert_eq!(resized.dimensions(), (384, 314));
    }

    #[test]
    fn test_decode_image_from_memory_with_fallback_decodes_jpeg_bytes() {
        let rgb = RgbImage::from_pixel(12, 9, Rgb([90, 140, 210]));
        let mut encoded = Vec::new();
        {
            let mut encoder = JpegEncoder::new_with_quality(&mut encoded, 85);
            encoder
                .encode_image(&DynamicImage::ImageRgb8(rgb))
                .expect("jpeg encoding should succeed");
        }
        // Simulate trailing garbage often seen in malformed files.
        encoded.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);

        let decoded = decode_image_from_memory_with_fallback(&encoded)
            .expect("fallback decoder should decode jpeg bytes");
        assert_eq!(decoded.dimensions(), (12, 9));
    }

    #[test]
    fn test_decode_image_from_memory_with_fallback_rejects_non_image_bytes() {
        let decoded = decode_image_from_memory_with_fallback(b"definitely-not-an-image");
        assert!(decoded.is_none());
    }

    #[test]
    fn test_decode_image_from_memory_with_fallback_decodes_png_bytes() {
        let source =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(7, 5, Rgba([8, 16, 24, 255])));
        let mut cursor = Cursor::new(Vec::<u8>::new());
        source
            .write_to(&mut cursor, ImageFormat::Png)
            .expect("png encoding should succeed");
        let encoded = cursor.into_inner();

        let decoded = decode_image_from_memory_with_fallback(&encoded)
            .expect("primary decoder should decode png bytes");
        assert_eq!(decoded.dimensions(), (7, 5));
    }
}
