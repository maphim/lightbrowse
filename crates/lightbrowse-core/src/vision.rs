//! Set-of-Mark (SoM) overlay — the "eyes" of human-like perception.
//!
//! Takes a screenshot + bounding boxes and draws numbered red frames over the
//! interactive elements, so a vision LLM can say "click number 7" exactly like
//! a human pointing at the screen. Numbers are rendered with a tiny embedded
//! 5×7 bitmap font — no font file, no extra binary weight.

use image::{Rgba, RgbaImage};

use crate::error::Result;
use crate::snapshot::{Bbox, SnapshotNode, SnapshotTree};

/// A numbered mark drawn onto the screenshot.
pub struct Mark {
    pub label: usize,
    pub bbox: Bbox,
}

const RED: Rgba<u8> = Rgba([220, 38, 38, 255]);
const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);

/// Classic 5×7 bitmap font, digits 0-9 (bit 4 = leftmost column).
const FONT: [[u8; 7]; 10] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110], // 0
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110], // 1
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111], // 2
    [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110], // 3
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010], // 4
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110], // 5
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110], // 6
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000], // 7
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110], // 8
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100], // 9
];

#[inline]
fn set(img: &mut RgbaImage, x: i64, y: i64, c: Rgba<u8>) {
    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
        img.put_pixel(x as u32, y as u32, c);
    }
}

/// Draw the label digits on a white chip at the box's top-left corner.
fn draw_label(img: &mut RgbaImage, x0: i64, y0: i64, label: usize) {
    let digits: Vec<u8> = label.to_string().bytes().map(|b| b - b'0').collect();
    let cw = 6i64; // digit width + spacing
    let chip_w = digits.len() as i64 * cw + 2;
    let chip_h = 9i64;
    for py in y0..(y0 + chip_h) {
        for px in x0..(x0 + chip_w) {
            set(img, px, py, WHITE);
        }
    }
    for (i, d) in digits.iter().enumerate() {
        let ox = x0 + 1 + i as i64 * cw;
        let oy = y0 + 1;
        for (row, bits_row) in FONT[*d as usize].iter().enumerate() {
            let bits = *bits_row;
            for col in 0..5usize {
                if bits & (1 << (4 - col as i32)) != 0 {
                    set(img, ox + col as i64, oy + row as i64, RED);
                }
            }
        }
    }
}

/// Overlay numbered frames on a PNG screenshot. Marks fully outside the
/// viewport or with zero area are skipped.
pub fn overlay(png: &[u8], marks: &[Mark]) -> Result<Vec<u8>> {
    let img = image::load_from_memory(png)
        .map_err(|e| crate::error::Error::Parse(format!("overlay decode: {e}")))?;
    let mut rgb = img.to_rgba8();
    let (w, h) = (rgb.width() as i64, rgb.height() as i64);

    for m in marks {
        let x0 = m.bbox.x.max(0.0) as i64;
        let y0 = m.bbox.y.max(0.0) as i64;
        let x1 = ((m.bbox.x + m.bbox.w).min(w as f64)) as i64;
        let y1 = ((m.bbox.y + m.bbox.h).min(h as f64)) as i64;
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        // 1px red frame.
        for px in x0..x1 {
            set(&mut rgb, px, y0, RED);
            set(&mut rgb, px, y1.saturating_sub(1), RED);
        }
        for py in y0..y1 {
            set(&mut rgb, x0, py, RED);
            set(&mut rgb, x1.saturating_sub(1), py, RED);
        }
        // Label chip pinned inside the frame.
        let (lx, ly) = (x0 + 2, y0 + 2);
        if lx + 12 < x1 && ly + 9 < y1 {
            draw_label(&mut rgb, lx, ly, m.label);
        }
    }

    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgb)
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| crate::error::Error::Parse(format!("overlay encode: {e}")))?;
    Ok(out.into_inner())
}

/// Pick interactive nodes (with bboxes) from a snapshot tree and assign SoM
/// labels in tree order. Returns (label, uid, text, bbox).
pub fn select_marks(tree: &SnapshotTree, max: usize) -> Vec<(usize, u64, String, Bbox)> {
    fn walk(n: &SnapshotNode, acc: &mut Vec<(u64, String, Bbox)>) {
        if crate::snapshot::is_interactive(n) {
            if let Some(b) = n.bbox {
                if b.w >= 1.0 && b.h >= 1.0 {
                    acc.push((n.uid, n.text.clone(), b));
                }
            }
        }
        for c in &n.children {
            walk(c, acc);
        }
    }
    let mut els = Vec::new();
    for n in &tree.nodes {
        walk(n, &mut els);
    }
    els.truncate(max);
    els.into_iter().enumerate().map(|(i, (uid, text, b))| (i + 1, uid, text, b)).collect()
}
