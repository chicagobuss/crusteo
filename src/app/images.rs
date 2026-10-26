//! Images read from daemons as they are needed, kept in memory for the run.
//!
//! A conversation asks for the images in the rows it draws, so only what an
//! open conversation scrolls to is read, and each file once per host however
//! many views show it. Large images are scaled down off the UI thread first.
//! The cache holds about [`CACHE_BYTES`] and forgets the least recently shown
//! beyond that. Crusteo has no cache directory, so nothing is kept on disk.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use fleet::HostId;
use fleet::images::ImageFile;
use fleet::prompt::ImageAttachment;
use gpui_kit::gpui::{App, Image, ImageFormat};

const CACHE_BYTES: usize = 64 * 1024 * 1024;
/// The longest side kept: sharp at 2x across a wide reply.
const MAX_SIDE: u32 = 1600;
/// After a host could not be asked, how long before asking again.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Images shown this recently are never forgotten to make room, so a
/// screenful bigger than the cache cannot make it read in circles.
const IN_VIEW: Duration = Duration::from_secs(5);

/// What a view shows for an image a daemon keeps.
#[derive(Clone)]
pub enum Shown {
    Loading,
    Ready(Arc<Image>),
    Unavailable,
}

/// Why an image could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The host answered: no such file, too big, not an image, or it does
    /// not serve files. Not asked again this run.
    Refused(String),
    /// The host could not be asked; asked again later.
    Unreachable,
}

enum Entry {
    Loading,
    Ready { image: Arc<Image>, used: Instant },
    Refused,
    Unreachable { until: Instant },
}

#[derive(Default)]
pub struct ImageCache {
    entries: HashMap<(HostId, ImageFile), Entry>,
    bytes: usize,
    /// Bumped whenever an image arrives or fails, for views to redraw.
    pub revision: u64,
}

impl ImageCache {
    /// What to show for `file` on `host`, and whether the caller should read
    /// it now: true once per file until that read lands.
    pub fn show(&mut self, host: &HostId, file: &ImageFile, now: Instant) -> (Shown, bool) {
        let key = (host.clone(), file.clone());
        match self.entries.get_mut(&key) {
            Some(Entry::Ready { image, used }) => {
                *used = now;
                (Shown::Ready(image.clone()), false)
            }
            Some(Entry::Loading) => (Shown::Loading, false),
            Some(Entry::Refused) => (Shown::Unavailable, false),
            Some(Entry::Unreachable { until }) if now < *until => (Shown::Unavailable, false),
            _ => {
                self.entries.insert(key, Entry::Loading);
                (Shown::Loading, true)
            }
        }
    }

    /// Records a read. Returns the images forgotten to make room, for GPUI
    /// to drop its decoded copies.
    pub fn finish(
        &mut self,
        host: &HostId,
        file: &ImageFile,
        result: Result<Arc<Image>, Failure>,
        now: Instant,
    ) -> Vec<Arc<Image>> {
        let key = (host.clone(), file.clone());
        let entry = match result {
            Ok(image) => {
                self.bytes += image.bytes.len();
                Entry::Ready { image, used: now }
            }
            Err(Failure::Refused(_)) => Entry::Refused,
            Err(Failure::Unreachable) => Entry::Unreachable {
                until: now + RETRY_AFTER,
            },
        };
        if let Some(Entry::Ready { image, .. }) = self.entries.insert(key, entry) {
            self.bytes -= image.bytes.len();
        }
        self.revision += 1;
        self.trim(now)
    }

    /// Forgets the least recently shown images until the cache fits.
    fn trim(&mut self, now: Instant) -> Vec<Arc<Image>> {
        let mut forgotten = Vec::new();
        while self.bytes > CACHE_BYTES {
            let oldest = (self.entries.iter())
                .filter_map(|(key, entry)| match entry {
                    Entry::Ready { used, .. } if now.duration_since(*used) > IN_VIEW => {
                        Some((*used, key))
                    }
                    _ => None,
                })
                .min_by_key(|(used, _)| *used)
                .map(|(_, key)| key.clone());
            let Some(key) = oldest else {
                break;
            };
            if let Some(Entry::Ready { image, .. }) = self.entries.remove(&key) {
                self.bytes -= image.bytes.len();
                forgotten.push(image);
            }
        }
        forgotten
    }

    #[cfg(test)]
    fn held(&self) -> usize {
        self.bytes
    }
}

/// Decodes an image read from a daemon and scales a large one down. Slow:
/// run it off the UI thread.
pub fn prepare(image: &ImageAttachment) -> Result<Arc<Image>, String> {
    let bytes = STANDARD
        .decode(image.data.as_bytes())
        .map_err(|e| e.to_string())?;
    let format =
        ImageFormat::from_mime_type(&image.mime_type).ok_or("This image type cannot be shown.")?;
    if format == ImageFormat::Svg {
        return Ok(Arc::new(Image::from_bytes(format, bytes)));
    }
    let reader = || image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format();
    let fits = reader()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
        .is_none_or(|(width, height)| width.max(height) <= MAX_SIDE);
    if fits {
        return Ok(Arc::new(Image::from_bytes(format, bytes)));
    }
    let mut reader = reader().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| e.to_string())?;
    let mut png = Cursor::new(Vec::new());
    decoded
        .thumbnail(MAX_SIDE, MAX_SIDE)
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(Image::from_bytes(
        ImageFormat::Png,
        png.into_inner(),
    )))
}

/// What a Markdown image shows while it loads, or when it cannot.
pub fn placeholder(loading: bool) -> Arc<Image> {
    static LOADING: OnceLock<Arc<Image>> = OnceLock::new();
    static UNAVAILABLE: OnceLock<Arc<Image>> = OnceLock::new();
    let (cell, mark) = if loading {
        (&LOADING, "")
    } else {
        (
            &UNAVAILABLE,
            r##"<path d="M62 46 L118 94" stroke="#8888" stroke-width="4" stroke-linecap="round"/>"##,
        )
    };
    cell.get_or_init(|| {
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="180" height="140" viewBox="0 0 180 140"><rect x="1" y="1" width="178" height="138" rx="8" fill="#8882" stroke="#8884"/><rect x="62" y="46" width="56" height="48" rx="4" fill="none" stroke="#8888" stroke-width="3"/><circle cx="76" cy="60" r="5" fill="#8888"/><path d="M66 90 L84 72 L96 84 L104 76 L114 90 Z" fill="#8888"/>{mark}</svg>"##
        );
        Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()))
    })
    .clone()
}

/// Opens a copy of an image in the system's viewer. The copy goes in the
/// temporary directory, named `name` with an extension for `mime_type`.
pub fn open_copy(name: &str, mime_type: &str, bytes: Vec<u8>, cx: &App) {
    let dir = std::env::temp_dir().join("crusteo-images");
    let path = dir.join(copy_name(name, mime_type));
    let written = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, bytes));
    if written.is_ok() {
        cx.open_with_system(&path);
    }
}

/// Opens a copy of an image read from a daemon, at full size.
pub fn open_read(name: &str, image: &ImageAttachment, cx: &App) {
    if let Ok(bytes) = STANDARD.decode(image.data.as_bytes()) {
        open_copy(name, &image.mime_type, bytes, cx);
    }
}

/// `name` made safe for a file name, with an extension that matches.
fn copy_name(name: &str, mime_type: &str) -> String {
    let mut name: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let extension = ImageFormat::from_mime_type(mime_type).map(|format| match format {
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Png => "png",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        _ => "img",
    });
    if let Some(extension) = extension
        && !name
            .to_ascii_lowercase()
            .ends_with(&format!(".{extension}"))
    {
        name = format!("{name}.{extension}");
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> ImageFile {
        ImageFile {
            cwd: "/".into(),
            path: format!("/k/{name}.png"),
        }
    }

    fn image(len: usize) -> Arc<Image> {
        Arc::new(Image::from_bytes(ImageFormat::Png, vec![0; len]))
    }

    #[test]
    fn each_file_is_read_once_until_it_lands() {
        let mut cache = ImageCache::default();
        let now = Instant::now();
        assert!(matches!(
            cache.show(&HostId::new("h"), &file("a"), now),
            (Shown::Loading, true)
        ));
        assert!(matches!(
            cache.show(&HostId::new("h"), &file("a"), now),
            (Shown::Loading, false)
        ));
        // The same file on another host is another image.
        assert!(matches!(
            cache.show(&HostId::new("g"), &file("a"), now),
            (Shown::Loading, true)
        ));
        cache.finish(&HostId::new("h"), &file("a"), Ok(image(10)), now);
        assert_eq!(cache.revision, 1);
        assert!(matches!(
            cache.show(&HostId::new("h"), &file("a"), now),
            (Shown::Ready(_), false)
        ));
        assert_eq!(cache.held(), 10);
    }

    #[test]
    fn a_refusal_sticks_and_an_unreachable_host_is_asked_again_later() {
        let mut cache = ImageCache::default();
        let now = Instant::now();
        cache.show(&HostId::new("h"), &file("gone"), now);
        cache.finish(
            &HostId::new("h"),
            &file("gone"),
            Err(Failure::Refused("nope".into())),
            now,
        );
        let later = now + Duration::from_secs(3600);
        assert!(matches!(
            cache.show(&HostId::new("h"), &file("gone"), later),
            (Shown::Unavailable, false)
        ));
        cache.show(&HostId::new("h"), &file("away"), now);
        cache.finish(
            &HostId::new("h"),
            &file("away"),
            Err(Failure::Unreachable),
            now,
        );
        assert!(matches!(
            cache.show(&HostId::new("h"), &file("away"), now),
            (Shown::Unavailable, false)
        ));
        assert!(matches!(
            cache.show(&HostId::new("h"), &file("away"), now + RETRY_AFTER),
            (Shown::Loading, true)
        ));
    }

    #[test]
    fn the_least_recently_shown_go_first_but_never_those_in_view() {
        let mut cache = ImageCache::default();
        let start = Instant::now();
        let big = CACHE_BYTES / 2;
        for (i, name) in ["a", "b"].into_iter().enumerate() {
            let at = start + Duration::from_secs(i as u64);
            cache.show(&HostId::new("h"), &file(name), at);
            cache.finish(&HostId::new("h"), &file(name), Ok(image(big)), at);
        }
        // "a" is shown again, so "b" is now the oldest.
        let later = start + Duration::from_secs(60);
        cache.show(&HostId::new("h"), &file("a"), later);
        cache.show(&HostId::new("h"), &file("c"), later);
        cache.finish(&HostId::new("h"), &file("c"), Ok(image(big)), later);
        assert!(matches!(
            cache.show(&HostId::new("h"), &file("b"), later),
            (Shown::Loading, true)
        ));
        assert!(matches!(
            cache.show(&HostId::new("h"), &file("a"), later),
            (Shown::Ready(_), false)
        ));
        assert_eq!(cache.held(), 2 * big);
        // Everything in view: over the cap rather than forgetting any.
        cache.finish(&HostId::new("h"), &file("b"), Ok(image(big)), later);
        assert_eq!(cache.held(), 3 * big);
    }

    #[test]
    fn large_images_are_scaled_down_and_small_ones_kept() {
        let png = |width, height| {
            let mut out = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(width, height)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            ImageAttachment::new(STANDARD.encode(out.into_inner()), "image/png")
        };
        let small = png(40, 30);
        let kept = prepare(&small).unwrap();
        assert_eq!(kept.bytes, STANDARD.decode(small.data.as_bytes()).unwrap());
        let scaled = prepare(&png(4000, 1000)).unwrap();
        let size = image::load_from_memory(&scaled.bytes).unwrap();
        assert_eq!((size.width(), size.height()), (MAX_SIDE, MAX_SIDE / 4));
        assert!(prepare(&ImageAttachment::new("!!", "image/png")).is_err());
        assert!(prepare(&ImageAttachment::new("AAAA", "image/x-unknown")).is_err());
    }

    #[test]
    fn copies_get_safe_names_with_a_matching_extension() {
        assert_eq!(copy_name("ab.png", "image/png"), "ab.png");
        assert_eq!(copy_name("shot", "image/jpeg"), "shot.jpg");
        assert_eq!(copy_name("a:b/c", "image/webp"), "a_b_c.webp");
    }
}
