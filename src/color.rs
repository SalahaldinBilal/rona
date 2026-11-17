use slint::{Color, ComponentHandle};

use crate::{AppWindow, ColorText, ParsedColor};

pub fn wire(app: &AppWindow) {
    let ui = app.global::<ColorText>();
    ui.on_parse(|text| match parse(&text) {
        Some(color) => ParsedColor { valid: true, color },
        None => ParsedColor { valid: false, color: Color::default() },
    });
    ui.on_format(|color| format(color).into());
}

/// Reads a pasted color: hex with or without `#` (3, 4, 6 or 8 digits), or CSS style `rgb()`, `rgba()`,
/// `hsl()`, `hsla()`, `hsv()` and `hsb()`, with commas or spaces and an optional `/ alpha`.
pub fn parse(text: &str) -> Option<Color> {
    let text = text.trim().to_ascii_lowercase();
    let Some((name, rest)) = text.split_once('(') else {
        return parse_hex(text.strip_prefix('#').unwrap_or(&text));
    };
    let ([first, second, third], alpha) = split_arguments(rest.strip_suffix(')')?)?;
    let alpha = match alpha {
        Some(alpha) => fraction(alpha, 1.0)?,
        None => 1.0,
    };
    let (red, green, blue) = match name.trim() {
        "rgb" | "rgba" => (fraction(first, 255.0)?, fraction(second, 255.0)?, fraction(third, 255.0)?),
        "hsl" | "hsla" => {
            let (saturation, value) = hsl_to_hsv(percent(second)?, percent(third)?);
            hsv_to_rgb(hue(first)?, saturation, value)
        }
        "hsv" | "hsva" | "hsb" | "hsba" => hsv_to_rgb(hue(first)?, percent(second)?, percent(third)?),
        _ => return None,
    };
    Some(Color::from_argb_u8(byte(alpha), byte(red), byte(green), byte(blue)))
}

pub fn format(color: Color) -> String {
    let rgb = format!("#{:02x}{:02x}{:02x}", color.red(), color.green(), color.blue());
    if color.alpha() == 255 { rgb } else { format!("{rgb}{:02x}", color.alpha()) }
}

fn parse_hex(hex: &str) -> Option<Color> {
    if !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    let digits: Vec<u8> = hex.bytes().map(|digit| (digit as char).to_digit(16).unwrap_or(0) as u8).collect();
    let channels: Vec<u8> = match digits.len() {
        3 | 4 => digits.iter().map(|digit| digit * 17).collect(),
        6 | 8 => digits.chunks(2).map(|pair| pair[0] * 16 + pair[1]).collect(),
        _ => return None,
    };
    Some(Color::from_argb_u8(channels.get(3).copied().unwrap_or(255), channels[0], channels[1], channels[2]))
}

fn split_arguments(inner: &str) -> Option<([&str; 3], Option<&str>)> {
    let (main, alpha) = match inner.split_once('/') {
        Some((main, alpha)) => (main, Some(alpha.trim())),
        None => (inner, None),
    };
    let parts: Vec<&str> =
        main.split(|character: char| character == ',' || character.is_whitespace()).filter(|part| !part.is_empty()).collect();
    match (parts.as_slice(), alpha) {
        (&[first, second, third], alpha) => Some(([first, second, third], alpha)),
        (&[first, second, third, alpha], None) => Some(([first, second, third], Some(alpha))),
        _ => None,
    }
}

fn number(text: &str) -> Option<f32> {
    text.parse::<f32>().ok().filter(|number| number.is_finite())
}

// A percentage, or a plain number out of `scale`, as 0 to 1.
fn fraction(text: &str, scale: f32) -> Option<f32> {
    let value = match text.strip_suffix('%') {
        Some(percentage) => number(percentage)? / 100.0,
        None => number(text)? / scale,
    };
    Some(value.clamp(0.0, 1.0))
}

// Saturation, lightness and value are percentages even without the `%`, as in CSS.
fn percent(text: &str) -> Option<f32> {
    Some((number(text.strip_suffix('%').unwrap_or(text))? / 100.0).clamp(0.0, 1.0))
}

fn hue(text: &str) -> Option<f32> {
    Some(number(text.strip_suffix("deg").unwrap_or(text))?.rem_euclid(360.0))
}

fn hsl_to_hsv(saturation: f32, lightness: f32) -> (f32, f32) {
    let value = lightness + saturation * lightness.min(1.0 - lightness);
    let saturation = if value == 0.0 { 0.0 } else { 2.0 * (1.0 - lightness / value) };
    (saturation, value)
}

fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> (f32, f32, f32) {
    let chroma = value * saturation;
    let sector = hue / 60.0;
    let second = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let (red, green, blue) = match sector as u32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let lift = value - chroma;
    (red + lift, green + lift, blue + lift)
}

fn byte(fraction: f32) -> u8 {
    (fraction * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argb(text: &str) -> Option<(u8, u8, u8, u8)> {
        parse(text).map(|color| (color.alpha(), color.red(), color.green(), color.blue()))
    }

    #[test]
    fn hex_copied_from_design_tools() {
        assert_eq!(argb("#6EA8FF"), Some((255, 110, 168, 255)));
        assert_eq!(argb("6ea8ff"), Some((255, 110, 168, 255)));
        assert_eq!(argb("#fff"), Some((255, 255, 255, 255)));
        assert_eq!(argb("#ffffff10"), Some((16, 255, 255, 255)));
        assert_eq!(argb("#f008"), Some((136, 255, 0, 0)));
    }

    #[test]
    fn css_rgb_in_old_and_new_syntax() {
        assert_eq!(argb("rgb(110, 168, 255)"), Some((255, 110, 168, 255)));
        assert_eq!(argb("rgba(255,255,255,0.5)"), Some((128, 255, 255, 255)));
        assert_eq!(argb("rgb(255 255 255 / 8%)"), Some((20, 255, 255, 255)));
        assert_eq!(argb("RGB(100%, 0%, 50%)"), Some((255, 255, 0, 128)));
    }

    #[test]
    fn hsl_and_hsv_match_their_rgb() {
        assert_eq!(argb("hsl(0, 100%, 50%)"), Some((255, 255, 0, 0)));
        assert_eq!(argb("hsl(120deg 100% 25%)"), Some((255, 0, 128, 0)));
        assert_eq!(argb("hsla(240, 100%, 50%, 0.5)"), Some((128, 0, 0, 255)));
        assert_eq!(argb("hsv(60, 100%, 100%)"), Some((255, 255, 255, 0)));
        assert_eq!(argb("hsl(0, 0%, 100%)"), Some((255, 255, 255, 255)));
    }

    #[test]
    fn half_typed_or_wrong_text_is_rejected() {
        for text in ["", "#", "#ff", "#fffff", "#ggg", "rgb(1, 2)", "rgb(1, 2, 3", "cmyk(0, 0, 0, 0)", "red"] {
            assert_eq!(argb(text), None, "{text}");
        }
    }

    #[test]
    fn format_keeps_alpha_only_when_needed() {
        assert_eq!(format(Color::from_argb_u8(255, 110, 168, 255)), "#6ea8ff");
        assert_eq!(format(Color::from_argb_u8(16, 255, 255, 255)), "#ffffff10");
    }
}
