//! Turns pasted clipboard images into files on disk that a Claude Code
//! session can `@`-mention.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use fs::Fs;
use gpui::{App, ClipboardEntry, Image, ImageFormat};

/// One attachment written to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedImage {
    pub path: PathBuf,
    /// `img-<n>.<ext>` — the name the mirrored copy keeps inside the
    /// worktree, so the `@` mention can be built without re-deriving it.
    pub file_name: String,
}

/// Reads images out of the clipboard, including image files copied from a
/// file manager.
///
/// Returns `None` when the clipboard leads with text: the source application
/// orders its offered flavors by preference, so text-first means the user is
/// pasting text and the caller must let the ordinary paste through.
pub fn read_clipboard_images(cx: &mut App) -> Option<Vec<Image>> {
    let clipboard = cx.read_from_clipboard()?;
    if matches!(
        clipboard.entries().first(),
        Some(ClipboardEntry::String(_)) | None
    ) {
        return None;
    }

    let mut images = Vec::new();
    let mut paths = Vec::new();
    for entry in clipboard.into_entries() {
        match entry {
            ClipboardEntry::Image(image) => images.push(image),
            ClipboardEntry::ExternalPaths(external) => paths.extend(external.paths().to_owned()),
            ClipboardEntry::String(_) => {}
        }
    }
    for path in paths {
        if let Some(image) = load_image_from_path(&path) {
            images.push(image);
        }
    }

    (!images.is_empty()).then_some(images)
}

fn load_image_from_path(path: &Path) -> Option<Image> {
    let content = std::fs::read(path).ok()?;
    let format = image_format_from_bytes(&content)?;
    Some(Image::from_bytes(format, content))
}

fn image_format_from_bytes(content: &[u8]) -> Option<ImageFormat> {
    match image::guess_format(content).ok()? {
        image::ImageFormat::Png => Some(ImageFormat::Png),
        image::ImageFormat::Jpeg => Some(ImageFormat::Jpeg),
        image::ImageFormat::WebP => Some(ImageFormat::Webp),
        image::ImageFormat::Gif => Some(ImageFormat::Gif),
        image::ImageFormat::Bmp => Some(ImageFormat::Bmp),
        image::ImageFormat::Tiff => Some(ImageFormat::Tiff),
        image::ImageFormat::Ico => Some(ImageFormat::Ico),
        image::ImageFormat::Pnm => Some(ImageFormat::Pnm),
        _ => None,
    }
}

/// The formats Claude Code accepts as an `@`-mentioned image. Anything else
/// has to be transcoded before it is worth writing out.
fn is_agent_readable(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::Webp
    )
}

/// Re-encodes `image` to PNG when its format is one Claude Code will not
/// read, and returns the bytes to write together with their extension.
///
/// This is the whole reason this module exists: a Windows screenshot reaches
/// the clipboard as `CF_DIB`, which `gpui_windows` surfaces as
/// [`ImageFormat::Bmp`], and an `@`-mentioned BMP loads no image at all —
/// verified against the real `claude` CLI, which answered "no image" for a
/// BMP and named the colour correctly for the same picture as PNG. Since the
/// Snipping Tool is *the* way a screenshot gets pasted here, this is the
/// common path, not a defensive one. Copying a picture out of a browser
/// yields PNG and hides the problem entirely, so the BMP branch must be
/// exercised explicitly in tests.
fn encode_for_agent(image: &Image) -> anyhow::Result<(Vec<u8>, &'static str)> {
    anyhow::ensure!(
        image.format != ImageFormat::Svg,
        "SVG images can't be attached to a ticket — paste a raster screenshot instead"
    );

    if is_agent_readable(image.format) {
        return Ok((image.bytes.clone(), image.format.extension()));
    }

    let source_format = match image.format {
        ImageFormat::Bmp => image::ImageFormat::Bmp,
        ImageFormat::Tiff => image::ImageFormat::Tiff,
        ImageFormat::Ico => image::ImageFormat::Ico,
        ImageFormat::Pnm => image::ImageFormat::Pnm,
        other => anyhow::bail!("unsupported pasted image format {other:?}"),
    };

    let decoded = image::load_from_memory_with_format(&image.bytes, source_format)
        .with_context(|| format!("failed to decode the pasted {source_format:?} image"))?;
    let mut encoded = Vec::new();
    decoded
        .write_to(
            &mut std::io::Cursor::new(&mut encoded),
            image::ImageFormat::Png,
        )
        .context("failed to re-encode the pasted image as PNG")?;
    Ok((encoded, "png"))
}

/// Writes `images` into `dir` as `img-<n>.<ext>`, numbering from
/// `start_index`. Decoding and encoding happen inline, so this must be
/// awaited from a background context.
pub async fn save_images(
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    start_index: usize,
    images: Vec<Image>,
) -> anyhow::Result<Vec<SavedImage>> {
    fs.create_dir(&dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;

    let mut saved = Vec::with_capacity(images.len());
    for (offset, image) in images.iter().enumerate() {
        let (bytes, extension) = encode_for_agent(image)?;
        let file_name = format!("img-{}.{extension}", start_index + offset);
        let path = dir.join(&file_name);
        fs.write(&path, &bytes)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        saved.push(SavedImage { path, file_name });
    }
    Ok(saved)
}

/// The number to give the next attachment, so reopening the modal for a
/// ticket that already has images appends rather than overwrites.
pub async fn next_image_index(fs: &Arc<dyn Fs>, dir: &Path) -> usize {
    let Ok(mut entries) = fs.read_dir(dir).await else {
        return 1;
    };
    let mut highest = 0;
    while let Some(Ok(entry)) = futures::StreamExt::next(&mut entries).await {
        let Some(stem) = entry.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if let Some(number) = stem
            .strip_prefix("img-")
            .and_then(|number| number.parse::<usize>().ok())
        {
            highest = highest.max(number);
        }
    }
    highest + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x2 BMP, encoded the way the Windows clipboard's `CF_DIB` path hands
    /// one to gpui.
    fn bitmap_2x2() -> Vec<u8> {
        let mut bytes = Vec::new();
        let pixels = image::RgbImage::from_fn(2, 2, |x, y| {
            image::Rgb([(x * 120) as u8, (y * 120) as u8, 40])
        });
        image::DynamicImage::ImageRgb8(pixels)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Bmp,
            )
            .expect("encoding a 2x2 bitmap should not fail");
        bytes
    }

    #[test]
    fn test_bitmaps_are_re_encoded_as_png() {
        let bitmap = bitmap_2x2();
        assert_eq!(image_format_from_bytes(&bitmap), Some(ImageFormat::Bmp));

        let (bytes, extension) = encode_for_agent(&Image::from_bytes(ImageFormat::Bmp, bitmap))
            .expect("a valid bitmap should re-encode");
        assert_eq!(extension, "png");
        assert_eq!(
            image::guess_format(&bytes).ok(),
            Some(image::ImageFormat::Png)
        );
    }

    #[test]
    fn test_agent_readable_formats_are_written_through_unchanged() {
        let png = b"\x89PNG\r\n\x1a\n and then some".to_vec();
        let (bytes, extension) =
            encode_for_agent(&Image::from_bytes(ImageFormat::Png, png.clone()))
                .expect("a png needs no transcoding");
        assert_eq!(extension, "png");
        assert_eq!(bytes, png);

        let (_, extension) = encode_for_agent(&Image::from_bytes(ImageFormat::Jpeg, vec![0xff]))
            .expect("a jpeg needs no transcoding");
        assert_eq!(extension, "jpg");
    }

    #[test]
    fn test_svg_is_rejected() {
        let error = encode_for_agent(&Image::from_bytes(ImageFormat::Svg, b"<svg/>".to_vec()))
            .expect_err("svg must be rejected");
        assert!(error.to_string().contains("SVG"), "{error}");
    }

    #[gpui::test]
    async fn test_save_images_numbers_from_the_start_index(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let dir = PathBuf::from(util::path!("/tickets/CT-1/images"));
        let saved = save_images(
            fs.clone(),
            dir.clone(),
            3,
            vec![Image::from_bytes(ImageFormat::Bmp, bitmap_2x2())],
        )
        .await
        .expect("saving should succeed");

        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].file_name, "img-3.png");
        assert_eq!(saved[0].path, dir.join("img-3.png"));

        let fs: Arc<dyn Fs> = fs;
        assert_eq!(next_image_index(&fs, &dir).await, 4);
    }
}
