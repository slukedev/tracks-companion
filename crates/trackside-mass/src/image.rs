//! Artwork URLs through Music Assistant's `/imageproxy/<id>?size=&fmt=` endpoint
//! (`controllers/metadata/images.py`). The endpoint needs no token.

use crate::endpoint::Endpoint;
use crate::models::Image;

/// The sizes the image proxy accepts (`_ALLOWED_IMAGEPROXY_SIZES`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageSize {
    #[default]
    Original,
    Px80,
    Px160,
    Px256,
    Px512,
    Px1024,
}

impl ImageSize {
    pub fn pixels(self) -> u32 {
        match self {
            Self::Original => 0,
            Self::Px80 => 80,
            Self::Px160 => 160,
            Self::Px256 => 256,
            Self::Px512 => 512,
            Self::Px1024 => 1024,
        }
    }

    /// The smallest allowed size of at least `pixels`, or the largest.
    pub fn at_least(pixels: u32) -> Self {
        [Self::Px80, Self::Px160, Self::Px256, Self::Px512]
            .into_iter()
            .find(|size| size.pixels() >= pixels)
            .unwrap_or(Self::Px1024)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Jpg,
    Png,
}

impl ImageFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Jpg => "jpg",
            Self::Png => "png",
        }
    }
}

fn valid_proxy_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The proxy URL for an image with a proxy id, else its own URL if it is public, else `None`.
pub fn image_url(
    endpoint: &Endpoint,
    image: &Image,
    size: ImageSize,
    format: Option<ImageFormat>,
) -> Option<String> {
    if let Some(id) = image.proxy_id.as_deref().filter(|id| valid_proxy_id(id)) {
        let mut url = format!(
            "{}/imageproxy/{}?size={}",
            endpoint.http_base(),
            id.to_ascii_lowercase(),
            size.pixels()
        );
        if let Some(format) = format {
            url.push_str("&fmt=");
            url.push_str(format.as_str());
        }
        return Some(url);
    }
    let public = image.path.starts_with("https://") || image.path.starts_with("http://");
    (image.remotely_accessible && public).then(|| image.path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(json: serde_json::Value) -> Image {
        serde_json::from_value(json).expect("decodes")
    }

    fn endpoint() -> Endpoint {
        Endpoint::parse("http://ma.local:8095").expect("parses")
    }

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn proxied_images_use_the_proxy_id() {
        let img = image(
            serde_json::json!({"type": "thumb", "path": "/x.jpg", "provider": "f", "proxy_id": ID}),
        );
        assert_eq!(
            image_url(&endpoint(), &img, ImageSize::Px256, Some(ImageFormat::Jpg)).as_deref(),
            Some(format!("http://ma.local:8095/imageproxy/{ID}?size=256&fmt=jpg").as_str())
        );
    }

    #[test]
    fn a_malformed_proxy_id_is_not_put_in_a_url() {
        let img = image(
            serde_json::json!({"type": "thumb", "path": "/x", "provider": "f", "proxy_id": "../../etc"}),
        );
        assert_eq!(
            image_url(&endpoint(), &img, ImageSize::Original, None),
            None
        );
    }

    #[test]
    fn public_images_without_a_proxy_id_use_their_own_url() {
        let img = image(serde_json::json!({
            "type": "thumb", "path": "https://i.scdn.co/image/ab", "provider": "spotify", "remotely_accessible": true
        }));
        assert_eq!(
            image_url(&endpoint(), &img, ImageSize::Original, None).as_deref(),
            Some("https://i.scdn.co/image/ab")
        );
    }

    #[test]
    fn sizes_round_up_to_an_allowed_one() {
        assert_eq!(ImageSize::at_least(200), ImageSize::Px256);
        assert_eq!(ImageSize::at_least(4000), ImageSize::Px1024);
    }
}
