//! A composer submission, kept intact across queueing and retries.
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachment {
    pub data: Arc<str>,
    pub mime_type: String,
    #[serde(skip)]
    content_key: u64,
}

impl ImageAttachment {
    pub fn new(data: impl Into<Arc<str>>, mime_type: impl Into<String>) -> Self {
        use std::hash::{Hash, Hasher};
        let data = data.into();
        let mime_type = mime_type.into();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        data.hash(&mut hash);
        mime_type.hash(&mut hash);
        Self {
            data,
            mime_type,
            content_key: hash.finish(),
        }
    }
    pub fn content_key(&self) -> u64 {
        self.content_key
    }
}
impl<'de> Deserialize<'de> for ImageAttachment {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WireImage {
            data: Arc<str>,
            mime_type: String,
        }
        let image = WireImage::deserialize(deserializer)?;
        Ok(Self::new(image.data, image.mime_type))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

impl Prompt {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn content_key_survives_wire_round_trip_and_tracks_contents() {
        let image = ImageAttachment::new("cG5n", "image/png");
        let restored: ImageAttachment =
            serde_json::from_value(serde_json::to_value(&image).unwrap()).unwrap();
        assert_eq!(image.content_key(), restored.content_key());
        assert!(!Arc::ptr_eq(&image.data, &restored.data));
        assert_ne!(
            image.content_key(),
            ImageAttachment::new("another", "image/png").content_key()
        );
        assert_ne!(
            image.content_key(),
            ImageAttachment::new("cG5n", "image/jpeg").content_key()
        );
    }

    #[test]
    fn image_only_prompt_is_sendable_and_keeps_wire_fields_when_steered() {
        let prompt = Prompt {
            text: String::new(),
            images: vec![ImageAttachment::new("cG5n", "image/png")],
        };
        assert!(!prompt.is_empty());
        let queued = prompt.clone();
        assert!(Arc::ptr_eq(&prompt.images[0].data, &queued.images[0].data));
        let message = crate::requests::send_message("agent", "dedupe-id", &queued, Some("steer"));
        assert_eq!(
            message["images"][0],
            json!({"data":"cG5n", "mimeType":"image/png"})
        );
        let initial = crate::requests::create_agent(
            "workspace",
            json!({"provider":"codex"}),
            Some(queued),
            "initial-id",
        );
        assert_eq!(initial["initialPrompt"], "");
        assert_eq!(initial["images"], message["images"]);
        assert_eq!(initial["clientMessageId"], "initial-id");
        assert_eq!(message["text"], "");
        assert_eq!(message["messageId"], "dedupe-id");
        assert_eq!(message["activeTurnBehavior"], "steer");
        assert!(
            Prompt {
                text: " \n".into(),
                ..Prompt::default()
            }
            .is_empty()
        );
    }
}
