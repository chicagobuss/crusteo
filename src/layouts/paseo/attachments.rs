//! Native clipboard images and the composer thumbnail strip.
use std::{io::Cursor, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD};
use fleet::prompt::ImageAttachment;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{Icon, Sizable};
use gpui_kit::gpui::{
    AnyElement, App, ClipboardEntry, ClipboardItem, Context, Image, ImageFormat, IntoElement,
    ParentElement, SharedString, Styled, WeakEntity, Window, div, img, px,
};

use crate::theme::{palette, size};

const MAX_BYTES: usize = 20 * 1024 * 1024;
const MAX_IMAGES: usize = 8;

#[derive(Clone)]
pub struct Attachment {
    pub wire: ImageAttachment,
    pub image: Arc<Image>,
}

#[derive(Default)]
pub struct AttachmentTray {
    pub images: Vec<Attachment>,
    pub error: Option<String>,
    pub pending: bool,
}
impl AttachmentTray {
    pub fn remove(&mut self, index: usize) {
        if index < self.images.len() {
            self.images.remove(index);
        }
        self.error = None;
    }
    pub fn restore(&mut self, wires: &[ImageAttachment]) -> bool {
        let images: Option<Vec<_>> = wires.iter().map(from_wire).collect();
        self.error = images
            .ok_or_else(|| "Could not restore these attachments.".into())
            .and_then(|images| append(&mut self.images, images))
            .err();
        self.error.is_none()
    }
}

pub trait HasAttachments: Sized + 'static {
    fn attachments(&mut self) -> &mut AttachmentTray;
    fn attachments_disabled(&self) -> bool {
        false
    }
}

pub fn paste_handler<T: HasAttachments>(
    this: WeakEntity<T>,
) -> impl Fn(&ClipboardItem, &mut Window, &mut App) -> bool {
    move |clipboard, _, cx| {
        if !clipboard
            .entries
            .iter()
            .any(|entry| matches!(entry, ClipboardEntry::Image(_)))
        {
            return false;
        }
        let _ = this.update(cx, |this, cx: &mut Context<T>| {
            if this.attachments_disabled() || this.attachments().pending {
                return;
            }
            let images: Vec<_> = clipboard
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    ClipboardEntry::Image(image) => Some(image.bytes.clone()),
                    _ => None,
                })
                .collect();
            let tray = this.attachments();
            tray.pending = true;
            tray.error = None;
            cx.notify();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        images
                            .iter()
                            .map(|bytes| normalize(bytes))
                            .collect::<Result<Vec<_>, String>>()
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let tray = this.attachments();
                    tray.pending = false;
                    tray.error = result
                        .and_then(|images| append(&mut tray.images, images))
                        .err();
                    cx.notify();
                });
            })
            .detach();
        });
        true
    }
}

impl AttachmentTray {
    pub fn prompt(&self, text: String) -> fleet::prompt::Prompt {
        fleet::prompt::Prompt {
            text,
            images: self.images.iter().map(|image| image.wire.clone()).collect(),
        }
    }
}

fn normalize(bytes: &[u8]) -> Result<Attachment, String> {
    if bytes.len() > MAX_BYTES {
        return Err("This clipboard image is too large (20 MB limit).".into());
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|e| format!("Could not read clipboard image: {e}"))?;
    let mut png = Cursor::new(Vec::new());
    decoded
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let png = png.into_inner();
    if png.len() > MAX_BYTES {
        return Err("This clipboard image is too large after conversion (20 MB limit).".into());
    }
    Ok(Attachment {
        wire: ImageAttachment::new(STANDARD.encode(&png), "image/png"),
        image: Arc::new(Image::from_bytes(ImageFormat::Png, png)),
    })
}

/// `None` leaves ordinary text paste to the textarea. Images take precedence
/// over text aliases such as a browser's copied image URL.
#[cfg(test)]
fn pasted_images(item: &ClipboardItem) -> Option<Result<Vec<Attachment>, String>> {
    let images: Vec<_> = item
        .entries
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(image.bytes.as_slice()),
            _ => None,
        })
        .collect();
    if images.is_empty() {
        return None;
    }
    Some(images.into_iter().map(normalize).collect())
}

fn append(current: &mut Vec<Attachment>, images: Vec<Attachment>) -> Result<(), String> {
    let bytes = current
        .iter()
        .chain(images.iter())
        .map(|image| image.image.bytes.len())
        .sum::<usize>();
    if current.len() + images.len() > MAX_IMAGES || bytes > MAX_BYTES {
        return Err("Attach up to 8 images, totaling at most 20 MB.".into());
    }
    current.extend(images);
    Ok(())
}

pub fn strip<T: HasAttachments>(images: &[Attachment], this: WeakEntity<T>) -> AnyElement {
    let p = palette();
    div()
        .flex()
        .flex_wrap()
        .gap(px(8.))
        .children(images.iter().enumerate().map(|(i, image)| {
            let this = this.clone();
            div()
                .relative()
                .child(
                    img(image.image.clone())
                        .w(px(72.))
                        .h(px(64.))
                        .rounded(px(6.)),
                )
                .child(
                    div().absolute().top(px(0.)).right(px(0.)).child(
                        Button::new(SharedString::from(format!("remove-image-{i}")))
                            .icon(Icon::empty().path("icons/x.svg"))
                            .ghost()
                            .xsmall()
                            .tooltip("Remove image")
                            .on_click(move |_, _, cx| {
                                let _ = this.update(cx, |this, cx| {
                                    if !this.attachments_disabled() {
                                        this.attachments().remove(i);
                                        cx.notify();
                                    }
                                });
                            }),
                    ),
                )
        }))
        .text_size(size::sm())
        .text_color(p.muted)
        .into_any_element()
}

pub fn decode(image: &ImageAttachment) -> Option<Arc<Image>> {
    let bytes = STANDARD.decode(image.data.as_bytes()).ok()?;
    let format = ImageFormat::from_mime_type(&image.mime_type)?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}
fn from_wire(wire: &ImageAttachment) -> Option<Attachment> {
    Some(Attachment {
        wire: wire.clone(),
        image: decode(wire)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_restore_keeps_the_existing_tray_when_it_would_exceed_limits() {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 3)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let attachment = normalize(png.get_ref()).unwrap();
        let mut tray = AttachmentTray {
            images: vec![attachment.clone(); MAX_IMAGES],
            error: None,
            pending: false,
        };
        assert!(!tray.restore(std::slice::from_ref(&attachment.wire)));
        assert_eq!(tray.images.len(), MAX_IMAGES);
        assert!(tray.error.is_some());
        tray.remove(0);
        assert!(tray.restore(&[attachment.wire]));
        assert_eq!(tray.images.len(), MAX_IMAGES);
        assert!(tray.error.is_none());
    }
    #[test]
    fn clipboard_png_is_preserved_as_sendable_image_and_text_paste_falls_through() {
        assert!(pasted_images(&ClipboardItem::new_string("hello".into())).is_none());
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 3)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let clipboard =
            ClipboardItem::new_image(&Image::from_bytes(ImageFormat::Png, png.into_inner()));
        let attachments = pasted_images(&clipboard).unwrap().unwrap();
        let bytes = STANDARD
            .decode(attachments[0].wire.data.as_bytes())
            .unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 3));
        assert_eq!(attachments[0].wire.mime_type, "image/png");
        assert!(normalize(b"broken").is_err());
        let mut full = vec![attachments[0].clone(); MAX_IMAGES];
        assert!(append(&mut full, attachments).is_err());
        assert_eq!(full.len(), MAX_IMAGES);
    }
}
