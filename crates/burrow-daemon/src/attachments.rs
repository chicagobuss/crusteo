//! Prompt images, kept so every client can show them.
//!
//! Paseo records only a prompt's text: its images go to the provider and
//! no client but the sender ever sees them. Burrow keeps each image as a
//! file in `<home>/attachments` (a temporary directory without a home),
//! named by a hash of its bytes, lists them in the `user_message` as
//! `images: [{path, mimeType}]`, and serves them back through
//! `file_explorer_request`, as Paseo serves the image files its agents save.
//! Nothing else is served from there.

use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use burrow_provider::Image;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::warn;

/// What a file outside the attachments is refused with: Paseo's words for
/// a path outside the workspace.
const OUTSIDE: &str = "Access outside of workspace is not allowed";

pub struct Attachments {
    dir: PathBuf,
}

impl Attachments {
    pub fn new(home: Option<&Path>, server_id: &str) -> Self {
        let dir = match home {
            Some(home) => home.join("attachments"),
            None => std::env::temp_dir().join(format!("burrow-attachments-{server_id}")),
        };
        Self { dir }
    }

    /// Keeps a prompt's images and returns the `user_message`'s `images`.
    /// One that cannot be decoded or written is left out, not the prompt.
    pub fn keep(&self, images: &[Image]) -> Vec<Value> {
        images
            .iter()
            .filter_map(|image| match self.keep_one(image) {
                Ok(path) => Some(json!({
                    "path": path.to_string_lossy(), "mimeType": image.mime_type,
                })),
                Err(error) => {
                    warn!(%error, "a prompt image was not kept");
                    None
                }
            })
            .collect()
    }

    fn keep_one(&self, image: &Image) -> Result<PathBuf, String> {
        let extension = extension(&image.mime_type).ok_or("not an image type")?;
        let bytes = STANDARD
            .decode(image.data.trim())
            .map_err(|e| e.to_string())?;
        let hash: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let path = self.dir.join(format!("{hash}.{extension}"));
        if !path.is_file() {
            // Write then rename, so a reader never sees half an image.
            let temp = self.dir.join(format!("{hash}.{extension}.tmp"));
            std::fs::write(&temp, &bytes).map_err(|e| e.to_string())?;
            std::fs::rename(&temp, &path).map_err(|e| e.to_string())?;
        }
        Ok(path)
    }

    /// `file_explorer_request` for a kept image: `path`, or `path` within
    /// `cwd`. Returns the reply's `file`, or its `error`.
    pub fn read(&self, cwd: &str, path: &str, max_bytes: Option<u64>) -> Result<Value, String> {
        let requested = Path::new(cwd).join(path);
        let file = requested
            .canonicalize()
            .map_err(|_| "File not found".to_string())?;
        let dir = self.dir.canonicalize().map_err(|_| OUTSIDE.to_string())?;
        let name = (file.parent() == Some(dir.as_path()))
            .then(|| file.file_name().and_then(|name| name.to_str()))
            .flatten()
            .filter(|name| kept_name(name))
            .ok_or(OUTSIDE)?;
        let mime = name
            .rsplit_once('.')
            .and_then(|(_, ext)| mime_type(ext))
            .ok_or(OUTSIDE)?;
        let metadata = std::fs::metadata(&file).map_err(|e| e.to_string())?;
        if max_bytes.is_some_and(|max| metadata.len() > max) {
            return Err("File is too large to display".into());
        }
        let bytes = std::fs::read(&file).map_err(|e| e.to_string())?;
        let modified = metadata
            .modified()
            .map(|at| crate::clock::iso(at.into()))
            .unwrap_or_else(|_| crate::clock::now_iso());
        Ok(json!({
            "path": path,
            "kind": "image",
            "encoding": "base64",
            "content": STANDARD.encode(&bytes),
            "mimeType": mime,
            "size": bytes.len(),
            "modifiedAt": modified,
        }))
    }
}

/// `<sha256 hex>.<extension>`, as [`Attachments::keep`] names a file.
fn kept_name(name: &str) -> bool {
    name.split_once('.').is_some_and(|(hash, extension)| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && mime_type(extension).is_some()
    })
}

const TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("tiff", "image/tiff"),
    ("avif", "image/avif"),
    ("svg", "image/svg+xml"),
];

fn extension(mime_type: &str) -> Option<&'static str> {
    let mime_type = match mime_type {
        "image/jpg" => "image/jpeg",
        other => other,
    };
    TYPES.iter().find(|(_, m)| *m == mime_type).map(|(e, _)| *e)
}

fn mime_type(extension: &str) -> Option<&'static str> {
    TYPES.iter().find(|(e, _)| *e == extension).map(|(_, m)| *m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (Attachments, PathBuf) {
        let home = std::env::temp_dir().join(format!("burrow-att-{}", crate::ids::agent_id()));
        (Attachments::new(Some(&home), "srv_test"), home)
    }

    fn image(data: &[u8], mime: &str) -> Image {
        Image {
            data: STANDARD.encode(data),
            mime_type: mime.into(),
        }
    }

    #[test]
    fn kept_images_are_listed_and_read_back() {
        let (attachments, home) = scratch();
        let listed = attachments.keep(&[
            image(b"one", "image/png"),
            image(b"one", "image/png"),
            image(b"two", "image/jpg"),
            image(b"x", "text/plain"),
            Image {
                data: "not base64!".into(),
                mime_type: "image/png".into(),
            },
        ]);
        assert_eq!(listed.len(), 3, "{listed:?}");
        // The same bytes are one file.
        assert_eq!(listed[0], listed[1]);
        assert_eq!(listed[2]["mimeType"], "image/jpg");
        let path = listed[2]["path"].as_str().unwrap();
        assert!(path.ends_with(".jpg"), "{path}");
        let root = if cfg!(windows) { &path[..3] } else { "/" };
        let file = attachments.read(root, path, None).unwrap();
        assert_eq!(file["kind"], "image");
        assert_eq!(file["encoding"], "base64");
        assert_eq!(file["mimeType"], "image/jpeg");
        assert_eq!(
            STANDARD.decode(file["content"].as_str().unwrap()).unwrap(),
            b"two"
        );
        assert_eq!(
            attachments.read(root, path, Some(2)),
            Err("File is too large to display".into())
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn nothing_else_is_served() {
        let (attachments, home) = scratch();
        let listed = attachments.keep(&[image(b"one", "image/png")]);
        let kept = PathBuf::from(listed[0]["path"].as_str().unwrap());
        let dir = kept.parent().unwrap();
        // Beside the images, under another name, or outside the directory.
        std::fs::write(dir.join("notes.png"), b"x").unwrap();
        std::fs::write(home.join("server-id"), b"srv").unwrap();
        for path in [
            dir.join("notes.png"),
            home.join("server-id"),
            dir.join("..").join("server-id"),
            dir.join("missing.png"),
        ] {
            let read = attachments.read("", &path.to_string_lossy(), None);
            assert!(read.is_err(), "{path:?} was served");
        }
        // A relative path is taken within `cwd`.
        let name = kept.file_name().unwrap().to_str().unwrap();
        assert!(attachments.read(&dir.to_string_lossy(), name, None).is_ok());
        std::fs::remove_dir_all(home).unwrap();
    }
}
