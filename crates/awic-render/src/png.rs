//! A view as PNG, for clients that cannot display SVG (live test F-09: Claude
//! Code saved the SVG image block to a file and showed nothing).
//!
//! The SVG stays the source: this rasterizes exactly the document the view
//! returns. Labels are drawn with the host's installed fonts — none is built
//! into the executable (the container installs `fonts-dejavu-core`). A host
//! without any still gets the picture, unlabelled, and a note saying why;
//! the numbers are in the view's metrics either way.

use std::sync::{Arc, OnceLock};

use resvg::{tiny_skia, usvg};

/// Monospace families tried in order for the views' `monospace` labels;
/// otherwise the first monospaced face installed, otherwise any face.
const PREFERRED: [&str; 4] = [
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Cascadia Mono",
];

/// A rasterized view.
#[derive(Debug, Clone)]
pub struct Png {
    pub bytes: Vec<u8>,
    /// Set when the picture could not be drawn as asked (no fonts installed).
    pub note: Option<String>,
}

/// The installed fonts, scanned once: the scan reads every font file the
/// host has, which is far too slow to repeat per picture.
fn fonts() -> &'static (Arc<usvg::fontdb::Database>, Option<String>) {
    static FONTS: OnceLock<(Arc<usvg::fontdb::Database>, Option<String>)> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        let family = |f: &usvg::fontdb::FaceInfo| f.families.first().map(|(n, _)| n.clone());
        let installed = |name: &str| {
            db.faces()
                .any(|f| f.families.iter().any(|(n, _)| n == name))
        };
        let chosen = PREFERRED
            .iter()
            .find(|n| installed(n))
            .map(|n| n.to_string())
            .or_else(|| db.faces().find(|f| f.monospaced).and_then(family))
            .or_else(|| db.faces().next().and_then(family));
        if let Some(name) = &chosen {
            db.set_monospace_family(name.clone());
            db.set_sans_serif_family(name.clone());
            db.set_serif_family(name.clone());
        }
        (Arc::new(db), chosen)
    })
}

/// Rasterizes an SVG document at its own size (one pixel per user unit) on an
/// opaque white page, as a PNG file.
pub fn svg_to_png(svg: &str) -> Result<Png, String> {
    let (db, family) = fonts();
    let options = usvg::Options {
        font_family: family.clone().unwrap_or_else(|| "monospace".to_string()),
        fontdb: Arc::clone(db),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(svg, &options).map_err(|e| format!("bad SVG: {e}"))?;
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or_else(|| format!("cannot draw a {}x{} picture", size.width(), size.height()))?;
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    let bytes = pixmap
        .encode_png()
        .map_err(|e| format!("PNG encoding failed: {e}"))?;
    let note = family.is_none().then(|| {
        "the PNG has no labels: no font is installed on the AWIC host (install e.g. \
         fonts-dejavu-core), or use the SVG"
            .to_string()
    });
    Ok(Png { bytes, note })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_text_with_an_installed_font() {
        let blank = r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="40" viewBox="0 0 120 40" font-family="ui-monospace, monospace"></svg>"##;
        let text = r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="40" viewBox="0 0 120 40" font-family="ui-monospace, monospace"><text x="4" y="28" font-size="20" fill="black">3.19 V</text></svg>"##;
        let decode = |png: &[u8]| tiny_skia::Pixmap::decode_png(png).unwrap();
        let b = decode(&svg_to_png(blank).unwrap().bytes);
        let drawn = svg_to_png(text).unwrap();
        let t = decode(&drawn.bytes);
        assert_eq!((t.width(), t.height()), (120, 40));
        assert!(
            b.pixels().iter().all(|p| p.red() == 255),
            "the page is white"
        );
        let ink = t.pixels().iter().filter(|p| p.red() < 128).count();
        if fonts().1.is_some() {
            assert!(ink > 100, "the label was not drawn: {ink} dark pixels");
            assert!(drawn.note.is_none());
        } else {
            // A host without fonts: still a picture, and a note saying why
            // it has no labels.
            assert!(drawn.note.is_some());
        }
    }

    #[test]
    fn a_broken_document_is_an_error() {
        assert!(svg_to_png("<svg").is_err());
    }
}
