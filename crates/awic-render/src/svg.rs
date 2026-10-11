//! A small SVG writer. Enough for axes, traces and labels, and nothing else.
//!
//! SVG rather than a raster format is a deliberate choice for this renderer's
//! main audience (SPECIFICATION.md §5.6): the annotations stay *text* in the
//! document, so a model reads the numbers exactly instead of inferring them
//! from pixels, and the same file is what the browser displays. It also means
//! no font table and no image codec in the tree.

use std::fmt::Write;

/// Accumulates SVG elements. Coordinates are user units = CSS pixels.
pub struct Svg {
    body: String,
    width: u32,
    height: u32,
}

/// Horizontal placement of a text anchor, spelled as SVG spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Start,
    Middle,
    End,
}

impl Anchor {
    fn as_str(self) -> &'static str {
        match self {
            Anchor::Start => "start",
            Anchor::Middle => "middle",
            Anchor::End => "end",
        }
    }
}

impl Svg {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            body: String::new(),
            width,
            height,
        }
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, fill: &str) -> &mut Self {
        let _ = write!(
            self.body,
            r#"<rect x="{}" y="{}" width="{}" height="{}" fill="{fill}"/>"#,
            n(x),
            n(y),
            n(w.max(0.0)),
            n(h.max(0.0))
        );
        self
    }

    pub fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, stroke: &str, w: f64) -> &mut Self {
        let _ = write!(
            self.body,
            r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{stroke}" stroke-width="{}"/>"#,
            n(x1),
            n(y1),
            n(x2),
            n(y2),
            n(w)
        );
        self
    }

    /// A dashed line, for the markers that say "this is a derived level, not
    /// a sample" — a reader should be able to tell those apart at a glance.
    pub fn dashed(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, stroke: &str) -> &mut Self {
        let _ = write!(
            self.body,
            r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{stroke}" stroke-width="1" stroke-dasharray="4 3"/>"#,
            n(x1),
            n(y1),
            n(x2),
            n(y2)
        );
        self
    }

    pub fn path(&mut self, d: &str, stroke: &str, fill: &str, w: f64) -> &mut Self {
        if d.is_empty() {
            return self;
        }
        let _ = write!(
            self.body,
            r#"<path d="{d}" fill="{fill}" stroke="{stroke}" stroke-width="{}"/>"#,
            n(w)
        );
        self
    }

    /// A filled path at a given opacity — the eye diagram's density layers.
    pub fn path_alpha(&mut self, d: &str, fill: &str, alpha: f64) -> &mut Self {
        if d.is_empty() {
            return self;
        }
        let _ = write!(
            self.body,
            r#"<path d="{d}" fill="{fill}" fill-opacity="{}" stroke="none"/>"#,
            n(alpha)
        );
        self
    }

    pub fn text(
        &mut self,
        x: f64,
        y: f64,
        s: &str,
        fill: &str,
        size: f64,
        anchor: Anchor,
    ) -> &mut Self {
        let _ = write!(
            self.body,
            r#"<text x="{}" y="{}" fill="{fill}" font-size="{}" text-anchor="{}">{}</text>"#,
            n(x),
            n(y),
            n(size),
            anchor.as_str(),
            escape(s)
        );
        self
    }

    /// Bold text, for titles and the one number a reader should see first.
    pub fn text_bold(
        &mut self,
        x: f64,
        y: f64,
        s: &str,
        fill: &str,
        size: f64,
        anchor: Anchor,
    ) -> &mut Self {
        let _ = write!(
            self.body,
            r#"<text x="{}" y="{}" fill="{fill}" font-size="{}" font-weight="600" text-anchor="{}">{}</text>"#,
            n(x),
            n(y),
            n(size),
            anchor.as_str(),
            escape(s)
        );
        self
    }

    /// Finishes the document. `desc` goes into `<desc>`, which is where a
    /// reader (or a model) finds the whole reading in words without having to
    /// look at the picture at all.
    pub fn finish(self, title: &str, desc: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"><title>{t}</title><desc>{d}</desc>{body}</svg>"#,
            w = self.width,
            h = self.height,
            t = escape(title),
            d = escape(desc),
            body = self.body
        )
    }
}

/// Numbers with at most two decimals and no trailing zeros: an SVG full of
/// `123.45000000000001` is both larger and harder to read.
fn n(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let rounded = (v * 100.0).round() / 100.0;
    let mut s = format!("{rounded}");
    if s == "-0" {
        s = "0".to_string();
    }
    s
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_escaped_so_a_channel_label_cannot_break_the_document() {
        let mut svg = Svg::new(10, 10);
        svg.text(0.0, 0.0, r#"<a & "b">"#, "#000", 10.0, Anchor::Start);
        let out = svg.finish("t", "d");
        assert!(out.contains("&lt;a &amp; &quot;b&quot;&gt;"), "{out}");
        assert!(!out.contains("<a "), "{out}");
    }

    #[test]
    fn the_description_carries_the_reading_in_words() {
        let svg = Svg::new(4, 2);
        let out = svg.finish("Eye", "1 kHz, eye height 3.1 V");
        assert!(
            out.contains("<desc>1 kHz, eye height 3.1 V</desc>"),
            "{out}"
        );
        assert!(out.contains(r#"viewBox="0 0 4 2""#), "{out}");
    }

    #[test]
    fn numbers_are_short_and_never_nan() {
        assert_eq!(n(1.0 / 3.0), "0.33");
        assert_eq!(n(12.0), "12");
        assert_eq!(n(f64::NAN), "0");
        assert_eq!(n(-0.001), "0");
    }
}
