//! Prompts as `agy` reads them. Its stream-json input takes text only, so
//! images go to private files and the prompt names them, as in Paseo.

use std::path::PathBuf;

use base64::Engine;
use burrow_provider::Image;

/// A private directory of one session's attached images, removed with it.
#[derive(Debug, Default)]
pub struct PromptFiles {
    directory: Option<PathBuf>,
}

impl PromptFiles {
    /// The text to send: the prompt, then one `Attached image: <path>` line
    /// per image.
    pub fn encode(&mut self, text: &str, images: &[Image]) -> Result<String, String> {
        let extensions = images
            .iter()
            .map(|image| extension(&image.mime_type))
            .collect::<Result<Vec<_>, _>>()?;
        let mut lines = Vec::new();
        if !text.is_empty() {
            lines.push(text.to_string());
        }
        for (image, extension) in images.iter().zip(extensions) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(image.data.trim())
                .map_err(|err| format!("Invalid image data: {err}"))?;
            let directory = self.directory()?;
            let file = directory.join(format!("{}.{extension}", uuid::Uuid::new_v4()));
            write_private(&file, &bytes)
                .map_err(|err| format!("Could not save an image: {err}"))?;
            lines.push(format!("Attached image: {}", file.display()));
        }
        Ok(lines.join("\n"))
    }

    fn directory(&mut self) -> Result<PathBuf, String> {
        if let Some(directory) = &self.directory {
            return Ok(directory.clone());
        }
        let directory = std::env::temp_dir().join(format!(
            "burrow-antigravity-images-{}",
            uuid::Uuid::new_v4().simple()
        ));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&directory)
            .map_err(|err| format!("Could not save an image: {err}"))?;
        self.directory = Some(directory.clone());
        Ok(directory)
    }
}

impl Drop for PromptFiles {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(bytes)
}

/// `image/<subtype>` to a file extension (`jpeg` is `jpg`, `svg+xml` is `svg`).
fn extension(mime_type: &str) -> Result<String, String> {
    let refused = || format!("Expected an image MIME type, received {mime_type}");
    let lower = mime_type.to_ascii_lowercase();
    let subtype = lower.strip_prefix("image/").ok_or_else(refused)?;
    let mut chars = subtype.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'));
    if !valid {
        return Err(refused());
    }
    Ok(match subtype {
        "jpeg" => "jpg".into(),
        "svg+xml" => "svg".into(),
        other => other.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(data: &str, mime: &str) -> Image {
        Image {
            data: data.into(),
            mime_type: mime.into(),
        }
    }

    #[test]
    fn images_become_private_files_named_in_the_prompt() {
        let mut files = PromptFiles::default();
        let text = files
            .encode(
                "Describe these.",
                &[image("aW1hZ2U=", "image/png"), image("aW1n", "image/jpeg")],
            )
            .unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Describe these.");
        let png = PathBuf::from(lines[1].strip_prefix("Attached image: ").unwrap());
        let jpg = PathBuf::from(lines[2].strip_prefix("Attached image: ").unwrap());
        assert_eq!(png.extension().unwrap(), "png");
        assert_eq!(jpg.extension().unwrap(), "jpg");
        assert_eq!(std::fs::read(&png).unwrap(), b"image");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode(png.parent().unwrap()) & 0o777, 0o700);
            assert_eq!(mode(&png) & 0o777, 0o600);
        }
        let directory = png.parent().unwrap().to_path_buf();
        drop(files);
        assert!(!directory.exists(), "removed with the session");
    }

    #[test]
    fn a_non_image_is_refused_before_anything_is_written() {
        let mut files = PromptFiles::default();
        let refused = files.encode("x", &[image("dGV4dA==", "text/plain")]);
        assert_eq!(
            refused.unwrap_err(),
            "Expected an image MIME type, received text/plain"
        );
        assert!(files.directory.is_none());
        assert_eq!(files.encode("only text", &[]).unwrap(), "only text");
    }
}
