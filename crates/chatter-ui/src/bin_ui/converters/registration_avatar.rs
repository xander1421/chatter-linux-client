use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};

pub struct DefaultAvatar {
    pub label: &'static str,
    pub image: Option<&'static [u8]>,
    pub color: [u8; 3],
}

/// Order and payloads shared by registration and the profile editor.
pub const DEFAULT_AVATARS: [DefaultAvatar; 6] = [
    DefaultAvatar {
        label: "Doge",
        image: Some(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/avatars/doge.jpg"
        ))),
        color: [0, 0, 0],
    },
    DefaultAvatar {
        label: "Pepe",
        image: Some(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/avatars/pepe.jpg"
        ))),
        color: [0, 0, 0],
    },
    DefaultAvatar {
        label: "Wojak",
        image: Some(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/avatars/wojak.jpg"
        ))),
        color: [0, 0, 0],
    },
    DefaultAvatar {
        label: "Teal initials",
        image: None,
        color: [15, 118, 110],
    },
    DefaultAvatar {
        label: "Blue initials",
        image: None,
        color: [29, 78, 216],
    },
    DefaultAvatar {
        label: "Purple initials",
        image: None,
        color: [109, 40, 217],
    },
];

pub fn random_preset_avatar_index() -> usize {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    nanos as usize % 3
}

pub fn default_avatar_bytes(index: usize, name: &str) -> Option<Vec<u8>> {
    let choice = DEFAULT_AVATARS.get(index)?;
    choice
        .image
        .map(|bytes| bytes.to_vec())
        .or_else(|| registration_avatar(name, choice.color))
}

/// Which preset `avatar` is, if any. Presets are stored and uploaded as the
/// exact bytes `default_avatar_bytes` returns (registration and the profile
/// editor both send them unprocessed), so equality identifies one; an initials
/// preset is matched against `name`'s rendering. `None` for a custom photo.
pub fn preset_avatar_index(avatar: &[u8], name: &str) -> Option<usize> {
    DEFAULT_AVATARS
        .iter()
        .position(|choice| choice.image == Some(avatar))
        .or_else(|| {
            (0..DEFAULT_AVATARS.len())
                .filter(|&i| DEFAULT_AVATARS[i].image.is_none())
                .find(|&i| default_avatar_bytes(i, name).as_deref() == Some(avatar))
        })
        .or_else(|| initials_preset_by_colour(avatar))
}

/// An initials preset rendered for an earlier name: a 256 px square whose
/// corner is exactly a preset colour. Without this a rename kept the old
/// letters, the stored image no longer matching the new name's rendering.
// matches on colour alone: a custom 256 px photo with an exact preset-coloured
// corner would be mistaken for the preset.
fn initials_preset_by_colour(avatar: &[u8]) -> Option<usize> {
    let image = image::load_from_memory_with_format(avatar, image::ImageFormat::Png).ok()?.to_rgba8();
    if image.dimensions() != (256, 256) {
        return None;
    }
    let corner = image.get_pixel(0, 0).0;
    DEFAULT_AVATARS.iter().position(|choice| {
        choice.image.is_none() && corner == [choice.color[0], choice.color[1], choice.color[2], 255]
    })
}

// Render the same initials shown by Avatar into real image bytes. Registration
// uploads these bytes, so friends and linked surfaces see the chosen identity.
pub fn registration_avatar(name: &str, color: [u8; 3]) -> Option<Vec<u8>> {
    const SIZE: u32 = 256;
    let font = FontArc::try_from_slice(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/fonts/Inter-SemiBold.ttf"
    )))
    .ok()?;
    let initials = super::initials_from_name(name);
    let text = if initials.is_empty() {
        "?"
    } else {
        initials.as_str()
    };
    let scale = PxScale::from(if text.chars().count() > 2 {
        92.0
    } else {
        112.0
    });
    let scaled = font.as_scaled(scale);
    let glyphs: Vec<_> = text.chars().map(|c| font.glyph_id(c)).collect();
    let width: f32 = glyphs.iter().map(|id| scaled.h_advance(*id)).sum();
    let mut x = (SIZE as f32 - width) / 2.0;
    let baseline = (SIZE as f32 + scaled.ascent() + scaled.descent()) / 2.0;
    let mut pixels =
        image::RgbaImage::from_pixel(SIZE, SIZE, image::Rgba([color[0], color[1], color[2], 255]));
    for id in glyphs {
        let glyph = id.with_scale_and_position(scale, point(x, baseline));
        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px >= 0 && py >= 0 && px < SIZE as i32 && py < SIZE as i32 {
                    let t = coverage;
                    let base = pixels.get_pixel_mut(px as u32, py as u32);
                    base.0 = [
                        ((base[0] as f32 * (1.0 - t)) + 255.0 * t) as u8,
                        ((base[1] as f32 * (1.0 - t)) + 255.0 * t) as u8,
                        ((base[2] as f32 * (1.0 - t)) + 255.0 * t) as u8,
                        255,
                    ];
                }
            });
        }
        x += scaled.h_advance(id);
    }
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(pixels)
        .write_to(&mut output, image::ImageFormat::Png)
        .ok()?;
    Some(output.into_inner())
}
