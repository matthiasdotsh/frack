// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

//! Burns freehand strokes into the PDF file as an additional content
//! stream. No annotation object is created; the page content itself is
//! extended – afterwards the strokes are a permanent part of the page
//! (undo only via external versioning, e.g. git-annex).

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct StrokePoint {
    /// Coordinates in the page's display coordinate system: origin at the
    /// top left, y pointing down, unit PDF points, page rotation already
    /// applied (i.e. as Poppler renders it).
    pub x: f64,
    pub y: f64,
    /// Pen pressure 0..=1 (0.5 = medium pressure for mouse/no sensor).
    pub pressure: f64,
}

#[derive(Clone, Debug)]
pub struct Stroke {
    pub points: Vec<StrokePoint>,
}

/// Burns strokes into the file. `strokes_by_page` is keyed by 0-based
/// page index. Writes atomically (temp file + rename).
pub fn burn_strokes(
    path: &Path,
    strokes_by_page: &BTreeMap<usize, Vec<Stroke>>,
    color: (f64, f64, f64),
    base_width: f64,
) -> Result<(), Box<dyn Error>> {
    if strokes_by_page.values().all(|s| s.is_empty()) {
        return Ok(());
    }
    let mut doc = Document::load(path)?;
    let pages = doc.get_pages();

    for (&page_idx, strokes) in strokes_by_page {
        if strokes.is_empty() {
            continue;
        }
        let page_id = *pages
            .get(&((page_idx + 1) as u32))
            .ok_or_else(|| format!("page {} not found", page_idx + 1))?;

        let bbox = page_box(&doc, page_id)?;
        let rotate = page_rotate(&doc, page_id);
        let ops = strokes_to_ops(strokes, bbox, rotate, color, base_width);

        let pre_id = doc.add_object(Stream::new(dictionary! {}, b"q\n".to_vec()));
        let post_id = doc.add_object(Stream::new(dictionary! {}, ops.into_bytes()));
        append_content(&mut doc, page_id, pre_id, post_id)?;
    }

    // Atomic save: temp file in the same directory, then rename.
    let tmp = path.with_extension("pdf.frack-tmp");
    doc.save(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Appends the new streams to the page content. The existing content is
/// wrapped in q…Q so that an unbalanced graphics state in the original
/// cannot displace our strokes.
fn append_content(
    doc: &mut Document,
    page_id: ObjectId,
    pre_id: ObjectId,
    post_id: ObjectId,
) -> Result<(), Box<dyn Error>> {
    let page_dict = doc
        .get_object_mut(page_id)
        .and_then(Object::as_dict_mut)?;
    let contents = page_dict.get(b"Contents").ok().cloned();
    let new = match contents {
        Some(Object::Reference(old)) => vec![
            Object::Reference(pre_id),
            Object::Reference(old),
            Object::Reference(post_id),
        ],
        Some(Object::Array(mut v)) => {
            v.insert(0, Object::Reference(pre_id));
            v.push(Object::Reference(post_id));
            v
        }
        None => vec![Object::Reference(pre_id), Object::Reference(post_id)],
        Some(other) => {
            return Err(format!("unexpected /Contents type: {other:?}").into());
        }
    };
    page_dict.set("Contents", Object::Array(new));
    Ok(())
}

fn strokes_to_ops(
    strokes: &[Stroke],
    bbox: [f64; 4],
    rotate: i64,
    color: (f64, f64, f64),
    base_width: f64,
) -> String {
    let mut ops = String::from("Q\nq\n1 J 1 j\n");
    ops.push_str(&format!(
        "{:.3} {:.3} {:.3} RG\n",
        color.0, color.1, color.2
    ));
    for stroke in strokes {
        match stroke.points.len() {
            0 => continue,
            1 => {
                // Single point: zero-length line with a round cap = a dot.
                let p = &stroke.points[0];
                let (x, y) = display_to_pdf(p.x, p.y, bbox, rotate);
                ops.push_str(&format!(
                    "{:.2} w\n{x:.2} {y:.2} m {x:.2} {y:.2} l S\n",
                    width_for(base_width, p.pressure),
                ));
            }
            _ => {
                for pair in stroke.points.windows(2) {
                    let (x1, y1) = display_to_pdf(pair[0].x, pair[0].y, bbox, rotate);
                    let (x2, y2) = display_to_pdf(pair[1].x, pair[1].y, bbox, rotate);
                    let w = width_for(base_width, (pair[0].pressure + pair[1].pressure) / 2.0);
                    ops.push_str(&format!(
                        "{w:.2} w\n{x1:.2} {y1:.2} m {x2:.2} {y2:.2} l S\n"
                    ));
                }
            }
        }
    }
    ops.push('Q');
    ops
}

/// Stroke width from base width and pressure (0..=1); 0.5 ≈ base width.
pub fn width_for(base_width: f64, pressure: f64) -> f64 {
    (base_width * (0.4 + 1.2 * pressure.clamp(0.0, 1.0))).max(0.2)
}

/// Converts a point from the (rotated) display coordinate system (origin
/// top left, y down) into the page's PDF user space (origin bottom left,
/// y up, relative to the crop box).
pub fn display_to_pdf(x: f64, y: f64, bbox: [f64; 4], rotate: i64) -> (f64, f64) {
    let [x0, y0, x1, y1] = bbox;
    let w = x1 - x0;
    let h = y1 - y0;
    // (a, b): point in unrotated page coordinates, top left, y down.
    let (a, b) = match rotate.rem_euclid(360) {
        90 => (y, h - x),
        180 => (w - x, h - y),
        270 => (w - y, x),
        _ => (x, y),
    };
    (x0 + a, y1 - b)
}

/// CropBox (if present, else MediaBox) – including inheritance through
/// the page tree. Poppler displays the crop box, so we map onto it.
fn page_box(doc: &Document, page_id: ObjectId) -> Result<[f64; 4], Box<dyn Error>> {
    let obj = inherited(doc, page_id, b"CropBox")
        .or_else(|| inherited(doc, page_id, b"MediaBox"))
        .ok_or("page has neither CropBox nor MediaBox")?;
    let arr = obj.as_array().map_err(|_| "Box ist kein Array")?;
    if arr.len() != 4 {
        return Err("box does not have 4 entries".into());
    }
    let mut v = [0f64; 4];
    for (i, o) in arr.iter().enumerate() {
        v[i] = as_f64(doc, o).ok_or("box entry is not a number")?;
    }
    // Normalize: (x0,y0) bottom left, (x1,y1) top right.
    Ok([
        v[0].min(v[2]),
        v[1].min(v[3]),
        v[0].max(v[2]),
        v[1].max(v[3]),
    ])
}

fn page_rotate(doc: &Document, page_id: ObjectId) -> i64 {
    inherited(doc, page_id, b"Rotate")
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0)
}

/// Looks up an entry in the page dictionary, inherited via /Parent if
/// necessary.
fn inherited(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    let mut id = page_id;
    for _ in 0..64 {
        let dict: &Dictionary = doc.get_dictionary(id).ok()?;
        if let Ok(obj) = dict.get(key) {
            // Resolve references (rare, but allowed).
            if let Ok(r) = obj.as_reference() {
                return doc.get_object(r).ok().cloned();
            }
            return Some(obj.clone());
        }
        id = dict.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

fn as_f64(doc: &Document, obj: &Object) -> Option<f64> {
    match obj {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        Object::Reference(r) => match doc.get_object(*r).ok()? {
            Object::Integer(i) => Some(*i as f64),
            Object::Real(r) => Some(*r as f64),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Forward transform (unrotated → display), the inverse of
    /// display_to_pdf, only for the round-trip test.
    fn pdf_to_display(u: f64, v: f64, bbox: [f64; 4], rotate: i64) -> (f64, f64) {
        let [x0, _y0, _x1, y1] = bbox;
        let w = bbox[2] - bbox[0];
        let h = bbox[3] - bbox[1];
        let a = u - x0;
        let b = y1 - v;
        match rotate.rem_euclid(360) {
            90 => (h - b, a),
            180 => (w - a, h - b),
            270 => (b, w - a),
            _ => (a, b),
        }
    }

    #[test]
    fn display_mapping_round_trip() {
        let bbox = [10.0, 20.0, 610.0, 812.0];
        for &rot in &[0i64, 90, 180, 270, 360, -90] {
            for &(u, v) in &[(10.0, 20.0), (610.0, 812.0), (100.0, 700.0), (300.5, 400.25)] {
                let (dx, dy) = pdf_to_display(u, v, bbox, rot);
                let (u2, v2) = display_to_pdf(dx, dy, bbox, rot);
                assert!(
                    (u - u2).abs() < 1e-9 && (v - v2).abs() < 1e-9,
                    "rot={rot} ({u},{v}) -> ({dx},{dy}) -> ({u2},{v2})"
                );
            }
        }
    }

    #[test]
    fn display_mapping_rot0_corners() {
        let bbox = [0.0, 0.0, 612.0, 792.0];
        // Top left of the display = (0, page height) in PDF space.
        assert_eq!(display_to_pdf(0.0, 0.0, bbox, 0), (0.0, 792.0));
        // Bottom right of the display = (width, 0).
        assert_eq!(display_to_pdf(612.0, 792.0, bbox, 0), (612.0, 0.0));
    }

    fn make_test_pdf(path: &Path) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content = lopdf::content::Content {
            operations: vec![lopdf::content::Operation::new(
                "re",
                vec![100.into(), 100.into(), 200.into(), 200.into()],
            )],
        };
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            content.encode().unwrap(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(path).unwrap();
    }

    #[test]
    fn burn_appends_content_stream() {
        let dir = std::env::temp_dir().join(format!("frack-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.pdf");
        make_test_pdf(&path);

        let mut strokes = BTreeMap::new();
        strokes.insert(
            0usize,
            vec![Stroke {
                points: vec![
                    StrokePoint { x: 50.0, y: 50.0, pressure: 0.5 },
                    StrokePoint { x: 150.0, y: 80.0, pressure: 0.7 },
                    StrokePoint { x: 250.0, y: 60.0, pressure: 0.4 },
                ],
            }],
        );
        burn_strokes(&path, &strokes, (0.8, 0.0, 0.0), 1.5).unwrap();

        // Reload and check: /Contents is now an array of
        // [q stream, original, stroke stream] and the new stream contains
        // our path operators.
        let doc = Document::load(&path).unwrap();
        let pages = doc.get_pages();
        let page_id = *pages.get(&1).unwrap();
        let dict = doc.get_dictionary(page_id).unwrap();
        let contents = dict.get(b"Contents").unwrap().as_array().unwrap();
        assert_eq!(contents.len(), 3);

        let last = doc
            .get_object(contents[2].as_reference().unwrap())
            .unwrap()
            .as_stream()
            .unwrap();
        let text = String::from_utf8_lossy(&last.content);
        assert!(text.contains(" m "), "stream has no moveto ops: {text}");
        assert!(text.contains(" RG"), "stream sets no color: {text}");
        assert!(text.starts_with("Q\nq"), "stream does not start with Q/q: {text}");

        // Burn a second time onto the already extended page (array case).
        burn_strokes(&path, &strokes, (0.8, 0.0, 0.0), 1.5).unwrap();
        let doc = Document::load(&path).unwrap();
        let dict = doc.get_dictionary(page_id).unwrap();
        let contents = dict.get(b"Contents").unwrap().as_array().unwrap();
        assert_eq!(contents.len(), 5);

        std::fs::remove_dir_all(&dir).ok();
    }
}
