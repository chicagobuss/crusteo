//! Images a daemon keeps as files rather than sending inline, and how to
//! read them back.
//!
//! Two kinds reach a conversation this way. Paseo saves an agent's image
//! output to a file and puts a Markdown image naming it in the reply. Burrow
//! keeps a prompt's images as files and lists them in the `user_message` as
//! `images: [{path, mimeType}]`. Paseo's own `user_message` carries no images
//! at all, so a prompt image sent from another client never reaches here.
//! Both kinds are read with `file_explorer_request`, as Paseo's app reads them.

use serde_json::Value;

use crate::prompt::ImageAttachment;

/// The largest image read. Its base64 must fit in one WebSocket frame, and
/// the daemon refuses anything bigger instead of sending it.
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// A file on a daemon's host, read with `file_explorer_request`: `path`
/// within `cwd`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageFile {
    pub cwd: String,
    pub path: String,
}

impl ImageFile {
    /// A file a `user_message` lists, `{path, mimeType}`; `None` for an
    /// inline image (`{data, mimeType}`) or a path that is not absolute.
    pub fn listed(image: &Value) -> Option<Self> {
        if image.get("data").is_some() {
            return None;
        }
        let path = image["path"].as_str()?.trim();
        Some(Self {
            cwd: filesystem_root(path)?,
            path: path.to_string(),
        })
    }

    /// The file a Markdown image's destination names, resolved as Paseo's
    /// app does: a `file:` URI or an absolute path is read from its
    /// filesystem's root, a home path from `~`, and a relative one from the
    /// agent's `cwd`. `None` for web and `data:` images, which need no daemon.
    pub fn from_source(source: &str, cwd: Option<&str>) -> Option<Self> {
        let source = source.trim();
        let scheme = source.split_once(':').map(|(s, _)| s.to_ascii_lowercase());
        if matches!(
            scheme.as_deref(),
            Some("http" | "https" | "data" | "blob" | "mailto")
        ) {
            return None;
        }
        let path = match source.strip_prefix("file://") {
            Some(rest) => file_uri_path(rest),
            None => source.to_string(),
        };
        if path.is_empty() {
            return None;
        }
        if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
            return Some(Self {
                cwd: "~".into(),
                path,
            });
        }
        if let Some(root) = filesystem_root(&path) {
            return Some(Self { cwd: root, path });
        }
        let cwd = cwd
            .map(str::trim)
            .filter(|cwd| filesystem_root(cwd).is_some())?;
        Some(Self {
            cwd: cwd.to_string(),
            path,
        })
    }

    pub fn request(&self) -> Value {
        crate::requests::read_file(&self.cwd, &self.path, MAX_IMAGE_BYTES)
    }

    /// The file's name, for a saved copy.
    pub fn file_name(&self) -> &str {
        self.path
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("image")
    }
}

/// The image in a `file_explorer_response` payload, or why there is none.
pub fn image_from_reply(payload: &Value) -> Result<ImageAttachment, String> {
    if let Some(error) = payload["error"].as_str() {
        return Err(error.to_string());
    }
    let file = &payload["file"];
    if file.is_null() {
        return Err("File unavailable.".into());
    }
    let mime = file["mimeType"].as_str().unwrap_or_default();
    match (
        file["kind"].as_str(),
        file["encoding"].as_str(),
        file["content"].as_str(),
    ) {
        (Some("image"), Some("base64"), Some(content))
            if mime.starts_with("image/") && !content.is_empty() =>
        {
            Ok(ImageAttachment::new(content, mime))
        }
        _ => Err("Not an image.".into()),
    }
}

/// The files a reply's Markdown images name, each once, and each such
/// image's destination with the index of its file. Web and `data:` images
/// are left out: they need no daemon.
pub fn markdown_files(text: &str, cwd: Option<&str>) -> (Vec<ImageFile>, Vec<(String, usize)>) {
    let mut files: Vec<ImageFile> = Vec::new();
    let mut sources = Vec::new();
    for source in markdown_images(text) {
        let Some(file) = ImageFile::from_source(&source, cwd) else {
            continue;
        };
        let index = match files.iter().position(|known| *known == file) {
            Some(index) => index,
            None => {
                files.push(file);
                files.len() - 1
            }
        };
        if !sources.iter().any(|(known, _)| *known == source) {
            sources.push((source, index));
        }
    }
    (files, sources)
}

/// The destinations of the Markdown images in `text`, `![alt](dest)` and
/// `<img src="dest">`, outside code. Backslash escapes are undone, as a
/// Markdown parser does, so each matches what a renderer is handed.
pub fn markdown_images(text: &str) -> Vec<String> {
    if !text.contains("![") && !text.contains("<img") {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = Some(&trimmed[..3]);
            continue;
        }
        line_images(line, &mut found);
    }
    found
}

fn line_images(line: &str, found: &mut Vec<String>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            // An inline code span: skip to the run that closes it.
            b'`' => {
                let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                let ticks = &line[i..i + run];
                i = match line[i + run..].find(ticks) {
                    Some(end) => i + run + end + run,
                    None => i + run,
                };
            }
            b'!' if bytes.get(i + 1) == Some(&b'[') => match image_destination(&line[i + 2..]) {
                Some((dest, used)) => {
                    found.push(dest);
                    i += 2 + used;
                }
                None => i += 2,
            },
            b'<' if bytes.len() > i + 4 && bytes[i + 1..i + 4].eq_ignore_ascii_case(b"img") => {
                let tag_end = line[i..].find('>').map_or(line.len(), |end| i + end);
                if let Some(src) = html_src(&line[i..tag_end]) {
                    found.push(src);
                }
                i = tag_end + 1;
            }
            _ => i += 1,
        }
    }
}

/// After `![`: the alt text, `](`, and the destination up to its `)` or
/// title. Returns the destination and how much of `rest` it took.
fn image_destination(rest: &str) -> Option<(String, usize)> {
    let mut depth = 0usize;
    let mut chars = rest.char_indices();
    let close = loop {
        let (at, c) = chars.next()?;
        match c {
            '\\' => {
                chars.next();
            }
            '[' => depth += 1,
            ']' if depth == 0 => break at,
            ']' => depth -= 1,
            _ => {}
        }
    };
    let after = rest[close + 1..].strip_prefix('(')?;
    let start = close + 2;
    let leading = after.len() - after.trim_start().len();
    let body = &after[leading..];
    if let Some(angled) = body.strip_prefix('<') {
        let end = angled.find('>')?;
        let tail = angled[end + 1..].find(')')?;
        return Some((
            unescape(&angled[..end]),
            start + leading + 1 + end + 1 + tail + 1,
        ));
    }
    let mut dest = String::new();
    let mut parens = 0usize;
    let mut chars = body.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some(&(_, next)) if next.is_ascii_punctuation() => {
                    dest.push(next);
                    chars.next();
                }
                _ => dest.push('\\'),
            },
            '(' => {
                parens += 1;
                dest.push(c);
            }
            ')' if parens == 0 => return Some((dest, start + leading + at + 1)),
            ')' => {
                parens -= 1;
                dest.push(c);
            }
            c if c.is_whitespace() => {
                // A title follows; the destination has ended.
                let tail = body[at..].find(')')?;
                return Some((dest, start + leading + at + tail + 1));
            }
            c => dest.push(c),
        }
    }
    None
}

fn html_src(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find("src=")? + 4;
    let value = &tag[at..];
    let src = match value.chars().next()? {
        quote @ ('"' | '\'') => value[1..].split(quote).next()?,
        _ => value
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()?,
    };
    (!src.is_empty()).then(|| src.to_string())
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(next)) if next.is_ascii_punctuation() => {
                out.push(*next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// The path a `file://` URI names, after its `file://`: `/C:/x` becomes
/// `C:/x`, and a host (`file://server/share/x`) a UNC path.
fn file_uri_path(rest: &str) -> String {
    let decoded = percent_decode(rest);
    if !rest.starts_with('/') {
        return format!("\\\\{}", decoded.replace('/', "\\"));
    }
    let bytes = decoded.as_bytes();
    if bytes.len() >= 3 && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return decoded[1..].to_string();
    }
    decoded
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(i + 1).and_then(|&b| hex(b)),
                bytes.get(i + 2).and_then(|&b| hex(b)),
            )
        {
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The root of the filesystem an absolute path is on: `/`, `C:/`, or a UNC
/// share. `None` for a relative path.
fn filesystem_root(path: &str) -> Option<String> {
    if path.starts_with("\\\\") || path.starts_with("//") {
        let parts: Vec<&str> = path[2..].split(['/', '\\']).take(2).collect();
        return (parts.len() == 2 && parts.iter().all(|p| !p.is_empty()))
            .then(|| format!("\\\\{}\\{}", parts[0], parts[1]));
    }
    if path.starts_with('/') {
        return Some("/".into());
    }
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\'))
    .then(|| format!("{}/", &path[..2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(cwd: &str, path: &str) -> Option<ImageFile> {
        Some(ImageFile {
            cwd: cwd.into(),
            path: path.into(),
        })
    }

    #[test]
    fn sources_resolve_as_paseos_app_reads_them() {
        // What Paseo's provider-image-output writes, on Linux and Windows.
        assert_eq!(
            ImageFile::from_source("file:///tmp/paseo-attachments-x1/ab%20c.png", None),
            file("/", "/tmp/paseo-attachments-x1/ab c.png")
        );
        assert_eq!(
            ImageFile::from_source("file:///C:/Users/sam/AppData/Local/Temp/a.png", None),
            file("C:/", "C:/Users/sam/AppData/Local/Temp/a.png")
        );
        assert_eq!(
            ImageFile::from_source("file://server/share/a.png", None),
            file("\\\\server\\share", "\\\\server\\share\\a.png")
        );
        assert_eq!(
            ImageFile::from_source("C:\\Temp\\a.png", None),
            file("C:/", "C:\\Temp\\a.png")
        );
        assert_eq!(
            ImageFile::from_source("~/shots/a.png", None),
            file("~", "~/shots/a.png")
        );
        assert_eq!(
            ImageFile::from_source("docs/a.png", Some("/home/dev/app")),
            file("/home/dev/app", "docs/a.png")
        );
        assert_eq!(ImageFile::from_source("docs/a.png", None), None);
        assert_eq!(ImageFile::from_source("docs/a.png", Some("relative")), None);
        for web in [
            "https://example.com/a.png",
            "HTTP://x/a.png",
            "data:image/png;base64,AA==",
        ] {
            assert_eq!(ImageFile::from_source(web, Some("/w")), None, "{web}");
        }
    }

    #[test]
    fn listed_files_are_absolute_paths_without_inline_data() {
        assert_eq!(
            ImageFile::listed(
                &json!({"path": "/home/dev/.burrow/attachments/ab.png", "mimeType": "image/png"})
            ),
            file("/", "/home/dev/.burrow/attachments/ab.png")
        );
        assert_eq!(
            ImageFile::listed(&json!({"data": "cG5n", "mimeType": "image/png"})),
            None
        );
        assert_eq!(ImageFile::listed(&json!({"path": "ab.png"})), None);
        assert_eq!(ImageFile::listed(&json!({"path": 3})), None);
        let request = ImageFile::listed(&json!({"path": "D:\\c\\attachments\\ab.png"}))
            .unwrap()
            .request();
        assert_eq!(request["type"], "file_explorer_request");
        assert_eq!(request["cwd"], "D:/");
        assert_eq!(request["mode"], "file");
        assert_eq!(request["maxBytes"], MAX_IMAGE_BYTES);
        assert!(request.get("acceptBinary").is_none());
    }

    #[test]
    fn replies_give_an_image_or_why_not() {
        let image = image_from_reply(&json!({"error": null, "file": {
            "kind": "image", "encoding": "base64", "content": "cG5n", "mimeType": "image/png"}}))
        .unwrap();
        assert_eq!(image, ImageAttachment::new("cG5n", "image/png"));
        assert_eq!(
            image_from_reply(&json!({"error": "File is too large to display", "file": null})),
            Err("File is too large to display".into())
        );
        assert!(image_from_reply(&json!({"error": null, "file": null})).is_err());
        assert!(
            image_from_reply(&json!({"file": {"kind": "binary", "encoding": "none",
                "mimeType": "application/octet-stream"}}))
            .is_err()
        );
        assert!(
            image_from_reply(&json!({"file": {"kind": "text", "encoding": "utf-8",
                "content": "hi", "mimeType": "text/plain"}}))
            .is_err()
        );
    }

    #[test]
    fn markdown_images_are_found_outside_code_and_unescaped() {
        // As Paseo renders a materialized image: escaped and on its own.
        let text = "Here it is:\n\n![Image](file:///tmp/paseo-attachments-x/a\\)b.png)\n\nDone.";
        assert_eq!(
            markdown_images(text),
            ["file:///tmp/paseo-attachments-x/a)b.png"]
        );
        let text = "![one](a.png \"title\") and ![two [nested]](<b c.png>) and \
                    <img src=\"d.png\" width=\"8\"> and `![code](no.png)`\n\
                    ```\n![fenced](no.png)\n```\n![last](e(1).png)";
        assert_eq!(
            markdown_images(text),
            ["a.png", "b c.png", "d.png", "e(1).png"]
        );
        assert!(markdown_images("no images [link](x.png) here").is_empty());
        assert!(markdown_images("![broken](").is_empty());
        assert!(markdown_images("ends with ![").is_empty());
    }

    #[test]
    fn a_replys_files_are_listed_once_with_every_destination_naming_them() {
        let text = "![a](file:///tmp/p/a.png) ![again](/tmp/p/a.png) ![web](https://example.com/w.png) \
                    ![rel](shots/b.png) ![a](file:///tmp/p/a.png)";
        let (files, sources) = markdown_files(text, Some("/home/dev/app"));
        assert_eq!(
            files,
            [
                file("/", "/tmp/p/a.png").unwrap(),
                file("/home/dev/app", "shots/b.png").unwrap()
            ]
        );
        assert_eq!(
            sources,
            [
                ("file:///tmp/p/a.png".to_string(), 0),
                ("/tmp/p/a.png".to_string(), 0),
                ("shots/b.png".to_string(), 1)
            ]
        );
        assert_eq!(markdown_files("plain text", None), (Vec::new(), Vec::new()));
    }

    #[test]
    fn file_names_come_from_either_separator() {
        assert_eq!(file("/", "/a/b/c.png").unwrap().file_name(), "c.png");
        assert_eq!(file("C:/", "C:\\a\\d.png").unwrap().file_name(), "d.png");
        assert_eq!(file("/", "/a/").unwrap().file_name(), "image");
    }
}
