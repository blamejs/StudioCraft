//! StudioCraft's app icon: the window icon, the picture in the update prompt and the icon of the
//! menu entry written beside an AppImage. Every size and format is in `assets/app-icon`.

/// The 128 px icon. The update prompt shows it, and the updater installs it as
/// `icons/hicolor/128x128/apps/ai.storyteller.studiocraft.png` next to an AppImage's menu entry.
pub const ICON_128_PNG: &[u8] = include_bytes!("../../../assets/app-icon/hicolor/128x128/apps/ai.storyteller.studiocraft.png");

/// The window icon on macOS: the 512 px icon on a transparent margin, for the Dock.
#[cfg(target_os = "macos")]
pub const WINDOW_PNG: &[u8] = include_bytes!("../../../assets/app-icon/studiocraft-macos-512.png");
/// The window icon on Windows, Linux and FreeBSD: the 256 px icon, for the title bar, the taskbar
/// and the window switcher.
#[cfg(not(target_os = "macos"))]
pub const WINDOW_PNG: &[u8] = include_bytes!("../../../assets/app-icon/hicolor/256x256/apps/ai.storyteller.studiocraft.png");

/// The window icon for `ViewportBuilder::with_icon`. `None` when [`WINDOW_PNG`] does not decode,
/// and the window then keeps the desktop's generic icon.
pub fn window_icon() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(WINDOW_PNG).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every PNG in `assets/app-icon`, with the side it must have.
    const FILES: &[(&str, &[u8], u32)] = &[
        ("16x16", include_bytes!("../../../assets/app-icon/hicolor/16x16/apps/ai.storyteller.studiocraft.png"), 16),
        ("24x24", include_bytes!("../../../assets/app-icon/hicolor/24x24/apps/ai.storyteller.studiocraft.png"), 24),
        ("32x32", include_bytes!("../../../assets/app-icon/hicolor/32x32/apps/ai.storyteller.studiocraft.png"), 32),
        ("48x48", include_bytes!("../../../assets/app-icon/hicolor/48x48/apps/ai.storyteller.studiocraft.png"), 48),
        ("64x64", include_bytes!("../../../assets/app-icon/hicolor/64x64/apps/ai.storyteller.studiocraft.png"), 64),
        ("128x128", include_bytes!("../../../assets/app-icon/hicolor/128x128/apps/ai.storyteller.studiocraft.png"), 128),
        ("256x256", include_bytes!("../../../assets/app-icon/hicolor/256x256/apps/ai.storyteller.studiocraft.png"), 256),
        ("512x512", include_bytes!("../../../assets/app-icon/hicolor/512x512/apps/ai.storyteller.studiocraft.png"), 512),
        ("macos-512", include_bytes!("../../../assets/app-icon/studiocraft-macos-512.png"), 512),
        ("1024", include_bytes!("../../../assets/app-icon/studiocraft-1024.png"), 1024),
    ];

    /// The Windows installer's icon.
    const ICO: &[u8] = include_bytes!("../../../assets/app-icon/studiocraft.ico");
    /// The macOS bundle icon.
    const ICNS: &[u8] = include_bytes!("../../../assets/app-icon/studiocraft.icns");

    fn u16_le(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }

    fn u32_le(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    fn u32_be(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn the_ico_holds_every_windows_size_as_a_png() {
        assert_eq!((u16_le(ICO, 0), u16_le(ICO, 2)), (0, 1), "an icon resource");
        let count = usize::from(u16_le(ICO, 4));
        let mut sides = Vec::new();
        for i in 0..count {
            let entry = 6 + 16 * i;
            let size = u32_le(ICO, entry + 8) as usize;
            let offset = u32_le(ICO, entry + 12) as usize;
            let icon = decode(&ICO[offset..offset + size]);
            let side = if ICO[entry] == 0 { 256 } else { u32::from(ICO[entry]) };
            assert_eq!((icon.width, icon.height), (side, side), "entry {i}");
            sides.push(side);
        }
        assert_eq!(sides, [16, 24, 32, 48, 64, 128, 256]);
    }

    #[test]
    fn the_icns_holds_every_iconset_size_as_a_png() {
        assert_eq!(&ICNS[..4], b"icns");
        assert_eq!(u32_be(ICNS, 4) as usize, ICNS.len());
        let mut found = Vec::new();
        let mut at = 8;
        while at < ICNS.len() {
            let kind = std::str::from_utf8(&ICNS[at..at + 4]).expect("an ASCII type code");
            let len = u32_be(ICNS, at + 4) as usize;
            let icon = decode(&ICNS[at + 8..at + len]);
            assert_eq!(icon.width, icon.height, "{kind}");
            found.push((kind.to_string(), icon.width));
            at += len;
        }
        let expected = [
            ("icp4", 16),
            ("icp5", 32),
            ("icp6", 64),
            ("ic07", 128),
            ("ic08", 256),
            ("ic09", 512),
            ("ic10", 1024),
            ("ic11", 32),
            ("ic12", 64),
            ("ic13", 256),
            ("ic14", 512),
        ];
        assert_eq!(found, expected.map(|(k, s)| (k.to_string(), s)));
    }

    fn decode(png: &[u8]) -> egui::IconData {
        eframe::icon_data::from_png_bytes(png).expect("the icon decodes as a PNG")
    }

    #[test]
    fn every_icon_size_decodes_at_its_size() {
        for (name, png, side) in FILES {
            let icon = decode(png);
            assert_eq!((icon.width, icon.height), (*side, *side), "{name}");
            assert_eq!(icon.rgba.len(), (side * side * 4) as usize, "{name}");
            // The rounded tile leaves the corners transparent and covers the middle.
            let alpha = |x: u32, y: u32| icon.rgba[((y * side + x) * 4 + 3) as usize];
            assert!(alpha(0, 0) < 32, "{name}: the corner is transparent");
            assert_eq!(alpha(side / 2, side / 2), 255, "{name}: the middle is opaque");
        }
    }

    #[test]
    fn every_icon_size_is_opaque_well_inside_the_tile() {
        for (name, png, side) in FILES {
            let icon = decode(png);
            // The macOS file holds the tile at 824/1024 of the side; the others fill the canvas.
            let tile = if name.starts_with("macos") { f64::from(*side) * 824.0 / 1024.0 } else { f64::from(*side) };
            let radius = 0.4 * tile;
            let center = f64::from(*side) / 2.0;
            for y in 0..*side {
                for x in 0..*side {
                    let (dx, dy) = (f64::from(x) + 0.5 - center, f64::from(y) + 0.5 - center);
                    if dx * dx + dy * dy <= radius * radius {
                        let alpha = icon.rgba[((y * side + x) * 4 + 3) as usize];
                        assert_eq!(alpha, 255, "{name}: pixel ({x}, {y}) inside the tile is opaque");
                    }
                }
            }
        }
    }

    #[test]
    fn the_window_icon_is_the_embedded_png() {
        let icon = window_icon().expect("the window icon decodes");
        let side = if cfg!(target_os = "macos") { 512 } else { 256 };
        assert_eq!((icon.width, icon.height), (side, side));
        assert_eq!(icon, decode(WINDOW_PNG));
    }

    #[test]
    fn the_update_prompt_and_the_appimage_menu_entry_get_the_128_px_icon() {
        let png = crate::UPDATE_APP.icon_png.expect("the updater has StudioCraft's icon");
        assert_eq!(png, ICON_128_PNG);
        let icon = decode(png);
        assert_eq!((icon.width, icon.height), (128, 128), "the updater installs it in the 128x128 folder");
        let entry = craft_update::desktop_entry::render(&crate::UPDATE_APP, std::path::Path::new("/opt/StudioCraft.AppImage"), true)
            .expect("a menu entry for a plain path");
        assert!(entry.contains(&format!("Icon={}\n", crate::BUNDLE_ID)), "{entry}");
    }
}
