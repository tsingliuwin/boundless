//! Slide pages: rectangular page frames laid out on the infinite canvas.
//!
//! A page is a world-space rectangle (Excalidraw-frame style): elements stay
//! ordinary scene elements; a page just declares "this region is slide N".
//! Flipping pages moves the camera; presenting zooms the camera to exactly
//! one page's rect. Pages are persisted in the scene file (`pages`, serde
//! default so older files load unchanged) but are board-level state, not in
//! undo history (same class as the canvas background).
//!
//! Pure data + layout math, no GPUI — unit tests below lock the invariants
//! (ratio parsing, gap-free horizontal growth, no overlap).

use serde::{Deserialize, Serialize};
use crate::scene::WBounds;

/// Gap between consecutive pages, world units. Wide enough that the
/// neighbors' content never bleeds into a page's exact-fit presentation view.
pub const PAGE_GAP: f64 = 240.0;

/// Aspect presets for new pages. Sizes are chosen so every preset fits the
/// AI's familiar 1600×1000 canvas comfortably and keeps a similar visual
/// area across ratios.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PageRatio {
    Ratio16_9,
    Ratio4_3,
    Ratio9_16,
    Ratio3_4,
    Ratio1_1,
}

impl PageRatio {
    /// Parse the tool-facing strings ("16:9", "4:3", …). Whitespace-tolerant.
    pub fn parse(s: &str) -> Option<PageRatio> {
        match s.replace(" ", "").as_str() {
            "16:9" => Some(PageRatio::Ratio16_9),
            "4:3" => Some(PageRatio::Ratio4_3),
            "9:16" => Some(PageRatio::Ratio9_16),
            "3:4" => Some(PageRatio::Ratio3_4),
            "1:1" => Some(PageRatio::Ratio1_1),
            _ => None,
        }
    }

    /// World-unit size (w, h) of one page at this ratio.
    pub fn size(&self) -> (f64, f64) {
        match self {
            PageRatio::Ratio16_9 => (1600.0, 900.0),
            PageRatio::Ratio4_3 => (1440.0, 1080.0),
            PageRatio::Ratio9_16 => (900.0, 1600.0),
            PageRatio::Ratio3_4 => (1080.0, 1440.0),
            PageRatio::Ratio1_1 => (1280.0, 1280.0),
        }
    }

    /// Chinese label for UI / tool echo.
    pub fn label(&self) -> &'static str {
        match self {
            PageRatio::Ratio16_9 => "16:9",
            PageRatio::Ratio4_3 => "4:3",
            PageRatio::Ratio9_16 => "9:16",
            PageRatio::Ratio3_4 => "3:4",
            PageRatio::Ratio1_1 => "1:1",
        }
    }

    /// The preset matching the given page size (2% aspect tolerance), so a
    /// manual "+ page" extends the deck in its existing ratio. Falls back to
    /// the 16:9 default when nothing matches (e.g. a resized page).
    pub fn from_size(w: f64, h: f64) -> PageRatio {
        let presets = [
            (PageRatio::Ratio16_9, 16.0 / 9.0),
            (PageRatio::Ratio4_3, 4.0 / 3.0),
            (PageRatio::Ratio9_16, 9.0 / 16.0),
            (PageRatio::Ratio3_4, 3.0 / 4.0),
            (PageRatio::Ratio1_1, 1.0),
        ];
        let ar = w / h;
        presets
            .into_iter()
            .find(|(_, r)| (ar - r).abs() / r <= 0.02)
            .map(|(p, _)| p)
            .unwrap_or_default()
    }
}

impl Default for PageRatio {
    fn default() -> Self {
        PageRatio::Ratio16_9
    }
}

/// One slide page: a titled world-space rectangle. Pages are numbered by
/// their Vec index (1-based); no separate id field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub title: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Page {
    pub fn bounds(&self) -> WBounds {
        WBounds::new(self.x, self.y, self.w, self.h)
    }
}

/// Compute the rect for the next page: pages grow horizontally from the
/// origin, top-aligned, separated by [`PAGE_GAP`]. The first page sits at
/// (0, 0) — the same origin the AI's default coordinate mental model uses.
pub fn next_page_rect(pages: &[Page], ratio: PageRatio) -> (f64, f64, f64, f64) {
    let (w, h) = ratio.size();
    let x = match pages.last() {
        Some(last) => last.x + last.w + PAGE_GAP,
        None => 0.0,
    };
    (x, 0.0, w, h)
}

/// Append a new page with the given title and return it (plus its 1-based
/// number) so the caller can report the rect back to the model.
pub fn push_page(pages: &mut Vec<Page>, title: Option<String>, ratio: PageRatio) -> (&Page, usize) {
    let (x, y, w, h) = next_page_rect(pages, ratio);
    let number = pages.len() + 1;
    let page = Page {
        title: match title {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => format!("第 {number} 页"),
        },
        x,
        y,
        w,
        h,
    };
    pages.push(page);
    (&pages[number - 1], number)
}

/// The page whose rect contains the world point (first match), if any.
pub fn page_at(pages: &[Page], x: f64, y: f64) -> Option<usize> {
    pages
        .iter()
        .position(|p| x >= p.x && x <= p.x + p.w && y >= p.y && y <= p.y + p.h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ratio_strings() {
        assert_eq!(PageRatio::parse("16:9"), Some(PageRatio::Ratio16_9));
        assert_eq!(PageRatio::parse(" 9:16 "), Some(PageRatio::Ratio9_16));
        assert_eq!(PageRatio::parse("4:3"), Some(PageRatio::Ratio4_3));
        assert_eq!(PageRatio::parse("21:9"), None);
        assert_eq!(PageRatio::parse("abc"), None);
    }

    #[test]
    fn pages_grow_horizontally_without_overlap() {
        let mut pages: Vec<Page> = Vec::new();
        let ratios = [
            PageRatio::Ratio16_9,
            PageRatio::Ratio9_16, // narrower page after a wide one
            PageRatio::Ratio4_3,
        ];
        for r in ratios {
            push_page(&mut pages, None, r);
        }
        assert_eq!(pages.len(), 3);
        // First page at the origin; each next starts after prev + gap.
        assert_eq!((pages[0].x, pages[0].y), (0.0, 0.0));
        assert_eq!(pages[0].w, 1600.0);
        assert_eq!(pages[1].x, 1600.0 + PAGE_GAP);
        assert_eq!(pages[2].x, pages[1].x + pages[1].w + PAGE_GAP);
        // All top-aligned.
        assert!(pages.iter().all(|p| p.y == 0.0));
        // No pairwise overlap.
        for i in 0..pages.len() {
            for j in i + 1..pages.len() {
                let (a, b) = (pages[i].bounds(), pages[j].bounds());
                assert!(!a.intersects(&b), "pages {i} and {j} overlap");
            }
        }
        // Default titles are the page numbers.
        assert_eq!(pages[1].title, "第 2 页");
    }

    #[test]
    fn titled_pages_keep_the_title() {
        let mut pages: Vec<Page> = Vec::new();
        let (p, n) = push_page(&mut pages, Some("  封面  ".into()), PageRatio::Ratio16_9);
        assert_eq!(p.title, "封面");
        assert_eq!(n, 1);
        // Blank titles fall back to the numbered default.
        let (p, n) = push_page(&mut pages, Some("  ".into()), PageRatio::Ratio16_9);
        assert_eq!(p.title, "第 2 页");
        assert_eq!(n, 2);
    }

    #[test]
    fn from_size_matches_presets_with_tolerance() {
        assert_eq!(PageRatio::from_size(1600.0, 900.0), PageRatio::Ratio16_9);
        assert_eq!(PageRatio::from_size(900.0, 1600.0), PageRatio::Ratio9_16);
        assert_eq!(PageRatio::from_size(1440.0, 1080.0), PageRatio::Ratio4_3);
        assert_eq!(PageRatio::from_size(1280.0, 1280.0), PageRatio::Ratio1_1);
        // ~1% off still matches; a genuinely different ratio falls back to 16:9.
        assert_eq!(PageRatio::from_size(1600.0, 910.0), PageRatio::Ratio16_9);
        assert_eq!(PageRatio::from_size(800.0, 600.0), PageRatio::Ratio4_3);
        assert_eq!(PageRatio::from_size(1234.0, 777.0), PageRatio::Ratio16_9);
    }

    #[test]
    fn page_at_finds_containing_page() {
        let mut pages: Vec<Page> = Vec::new();
        push_page(&mut pages, None, PageRatio::Ratio16_9);
        push_page(&mut pages, None, PageRatio::Ratio16_9);
        assert_eq!(page_at(&pages, 800.0, 450.0), Some(0));
        assert_eq!(page_at(&pages, 1600.0 + PAGE_GAP + 100.0, 100.0), Some(1));
        assert_eq!(page_at(&pages, 1600.0 + PAGE_GAP / 2.0, 100.0), None); // in the gap
    }
}
