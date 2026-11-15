//! QR codes as SVG, rendered by the server so the dashboard loads nothing from elsewhere.

use qrcode::render::svg;
use qrcode::{EcLevel, QrCode};

/// An SVG QR code for `text`, black on white with a quiet zone, or `None` when the text is
/// too long for a QR code.
pub fn qr_svg(text: &str) -> Option<String> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M).ok()?;
    Some(
        code.render::<svg::Color<'_>>()
            .min_dimensions(256, 256)
            .quiet_zone(true)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_an_svg() {
        let svg = qr_svg("trackside://pair?d=eyJ2IjoxfQ").unwrap_or_default();
        assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"), "{svg}");
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn text_too_long_for_a_code_is_none() {
        assert_eq!(qr_svg(&"x".repeat(8_000)), None);
    }
}
