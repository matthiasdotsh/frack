// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

//! Saves freehand strokes as standard PDF ink annotations (ISO 32000,
//! annotation subtype "Ink"). Every stroke becomes its own annotation
//! object carrying the path coordinates (/InkList), colour and width –
//! stored separately from the page content, which stays untouched. Any
//! standard PDF editor can therefore delete or move individual strokes
//! later. A pre-rendered appearance stream (/AP) preserves the
//! pressure-dependent line width in every viewer.

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

/// Saves strokes into the file as ink annotations. `strokes_by_page` is
/// keyed by 0-based page index. Writes atomically (temp file + rename).
pub fn save_strokes(
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
        let mut annot_ids = Vec::new();
        for stroke in strokes {
            if stroke.points.is_empty() {
                continue;
            }
            annot_ids.push(add_ink_annotation(
                &mut doc, stroke, bbox, rotate, color, base_width,
            ));
        }
        append_annots(&mut doc, page_id, annot_ids)?;
    }

    // Atomic save: temp file in the same directory, then rename.
    let tmp = path.with_extension("pdf.frack-tmp");
    doc.save(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Creates one /Ink annotation (plus its appearance form XObject) for a
/// single stroke and returns its object id. The caller adds the id to
/// the page's /Annots array.
fn add_ink_annotation(
    doc: &mut Document,
    stroke: &Stroke,
    bbox: [f64; 4],
    rotate: i64,
    color: (f64, f64, f64),
    base_width: f64,
) -> ObjectId {
    let pts: Vec<(f64, f64)> = stroke
        .points
        .iter()
        .map(|p| display_to_pdf(p.x, p.y, bbox, rotate))
        .collect();

    // /Rect: bounding box of the path, padded by half of the widest
    // segment so round caps are not clipped.
    let max_w = stroke
        .points
        .iter()
        .map(|p| width_for(base_width, p.pressure))
        .fold(0.0_f64, f64::max);
    let pad = max_w / 2.0 + 1.0;
    let mut rect = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for &(x, y) in &pts {
        rect[0] = rect[0].min(x);
        rect[1] = rect[1].min(y);
        rect[2] = rect[2].max(x);
        rect[3] = rect[3].max(y);
    }
    rect = [rect[0] - pad, rect[1] - pad, rect[2] + pad, rect[3] + pad];
    let rect_arr =
        || Object::Array(rect.iter().map(|&v| Object::Real(v as f32)).collect());

    // Appearance stream: reproduces the pressure-dependent widths
    // exactly as drawn on screen. With /BBox equal to /Rect and no
    // /Matrix the form's coordinates are page coordinates. Editors that
    // regenerate the appearance fall back to /InkList and the /BS width.
    let form_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => rect_arr(),
        },
        stroke_ops(stroke, &pts, color, base_width).into_bytes(),
    ));

    let ink: Vec<Object> = pts
        .iter()
        .flat_map(|&(x, y)| [Object::Real(x as f32), Object::Real(y as f32)])
        .collect();
    doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Ink",
        "Rect" => rect_arr(),
        "InkList" => vec![Object::Array(ink)],
        "C" => Object::Array(vec![
            Object::Real(color.0 as f32),
            Object::Real(color.1 as f32),
            Object::Real(color.2 as f32),
        ]),
        "BS" => dictionary! {
            "Type" => "Border",
            "S" => "S",
            "W" => Object::Real(base_width as f32),
        },
        // Print flag: the annotation appears when the PDF is printed.
        "F" => 4,
        "AP" => dictionary! { "N" => Object::Reference(form_id) },
    })
}

/// Appends annotation references to the page's /Annots array (which may
/// be missing, inline, or an indirect reference).
fn append_annots(
    doc: &mut Document,
    page_id: ObjectId,
    ids: Vec<ObjectId>,
) -> Result<(), Box<dyn Error>> {
    let page_dict = doc.get_dictionary(page_id)?;
    let refs = ids.into_iter().map(Object::Reference);
    match page_dict.get(b"Annots").ok().cloned() {
        Some(Object::Reference(r)) => {
            let arr = doc.get_object_mut(r).and_then(Object::as_array_mut)?;
            arr.extend(refs);
        }
        Some(Object::Array(mut v)) => {
            v.extend(refs);
            set_annots(doc, page_id, v)?;
        }
        None => set_annots(doc, page_id, refs.collect())?,
        Some(other) => {
            return Err(format!("unexpected /Annots type: {other:?}").into());
        }
    }
    Ok(())
}

fn set_annots(
    doc: &mut Document,
    page_id: ObjectId,
    v: Vec<Object>,
) -> Result<(), Box<dyn Error>> {
    doc.get_object_mut(page_id)
        .and_then(Object::as_dict_mut)?
        .set("Annots", Object::Array(v));
    Ok(())
}

/// Path operators for one stroke, in page coordinates (`pts` are the
/// already transformed points of `stroke`).
fn stroke_ops(
    stroke: &Stroke,
    pts: &[(f64, f64)],
    color: (f64, f64, f64),
    base_width: f64,
) -> String {
    let mut ops = String::from("q\n1 J 1 j\n");
    ops.push_str(&format!(
        "{:.3} {:.3} {:.3} RG\n",
        color.0, color.1, color.2
    ));
    if pts.len() == 1 {
        // Single point: zero-length line with a round cap = a dot.
        let (x, y) = pts[0];
        ops.push_str(&format!(
            "{:.2} w\n{x:.2} {y:.2} m {x:.2} {y:.2} l S\n",
            width_for(base_width, stroke.points[0].pressure),
        ));
    } else {
        for i in 0..pts.len() - 1 {
            let (x1, y1) = pts[i];
            let (x2, y2) = pts[i + 1];
            let w = width_for(
                base_width,
                (stroke.points[i].pressure + stroke.points[i + 1].pressure) / 2.0,
            );
            ops.push_str(&format!(
                "{w:.2} w\n{x1:.2} {y1:.2} m {x2:.2} {y2:.2} l S\n"
            ));
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

    fn test_strokes() -> BTreeMap<usize, Vec<Stroke>> {
        let mut strokes = BTreeMap::new();
        strokes.insert(
            0usize,
            vec![
                Stroke {
                    points: vec![
                        StrokePoint { x: 50.0, y: 50.0, pressure: 0.5 },
                        StrokePoint { x: 150.0, y: 80.0, pressure: 0.7 },
                        StrokePoint { x: 250.0, y: 60.0, pressure: 0.4 },
                    ],
                },
                Stroke {
                    points: vec![StrokePoint { x: 300.0, y: 400.0, pressure: 0.9 }],
                },
            ],
        );
        strokes
    }

    #[test]
    fn save_creates_ink_annotations_and_leaves_content_alone() {
        let dir = std::env::temp_dir().join(format!("frack-test-annot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.pdf");
        make_test_pdf(&path);

        save_strokes(&path, &test_strokes(), (0.8, 0.0, 0.0), 1.5).unwrap();

        let doc = Document::load(&path).unwrap();
        let pages = doc.get_pages();
        let page_id = *pages.get(&1).unwrap();
        let dict = doc.get_dictionary(page_id).unwrap();

        // The page content is untouched – annotations are separate objects.
        assert!(
            dict.get(b"Contents").unwrap().as_reference().is_ok(),
            "/Contents is no longer the original single stream"
        );

        // One /Ink annotation per stroke.
        let annots = dict.get(b"Annots").unwrap().as_array().unwrap();
        assert_eq!(annots.len(), 2);
        for a in annots {
            let ad = doc.get_dictionary(a.as_reference().unwrap()).unwrap();
            assert_eq!(ad.get(b"Subtype").unwrap().as_name().unwrap(), b"Ink");
            let ink = ad.get(b"InkList").unwrap().as_array().unwrap();
            assert_eq!(ink.len(), 1, "one path per stroke");
            assert!(ad.get(b"Rect").unwrap().as_array().unwrap().len() == 4);
            assert!(ad.get(b"C").is_ok() && ad.get(b"AP").is_ok());
        }

        // First stroke: 3 points = 6 coordinates; appearance stream has
        // colour and moveto ops.
        let a0 = doc
            .get_dictionary(annots[0].as_reference().unwrap())
            .unwrap();
        let path0 = a0.get(b"InkList").unwrap().as_array().unwrap()[0]
            .as_array()
            .unwrap();
        assert_eq!(path0.len(), 6);
        let ap = a0.get(b"AP").unwrap().as_dict().unwrap();
        let form = doc
            .get_object(ap.get(b"N").unwrap().as_reference().unwrap())
            .unwrap()
            .as_stream()
            .unwrap();
        let text = String::from_utf8_lossy(&form.content);
        assert!(text.contains(" m "), "appearance has no moveto ops: {text}");
        assert!(text.contains(" RG"), "appearance sets no colour: {text}");

        // Saving again appends to the existing /Annots array.
        save_strokes(&path, &test_strokes(), (0.8, 0.0, 0.0), 1.5).unwrap();
        let doc = Document::load(&path).unwrap();
        let dict = doc.get_dictionary(page_id).unwrap();
        assert_eq!(dict.get(b"Annots").unwrap().as_array().unwrap().len(), 4);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Deleting the annotations (as any external PDF editor would)
    /// restores a page that is structurally identical to the original –
    /// the strokes leave no trace in the content stream.
    #[test]
    fn annotations_are_deletable() {
        let dir = std::env::temp_dir().join(format!("frack-test-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.pdf");
        make_test_pdf(&path);
        let original_content = page_content(&path);

        save_strokes(&path, &test_strokes(), (0.8, 0.0, 0.0), 1.5).unwrap();

        let mut doc = Document::load(&path).unwrap();
        let page_id = *doc.get_pages().get(&1).unwrap();
        doc.get_object_mut(page_id)
            .and_then(Object::as_dict_mut)
            .unwrap()
            .remove(b"Annots");
        doc.save(&path).unwrap();

        assert_eq!(page_content(&path), original_content);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Decoded bytes of the page's (single) content stream.
    fn page_content(path: &Path) -> Vec<u8> {
        let doc = Document::load(path).unwrap();
        let page_id = *doc.get_pages().get(&1).unwrap();
        doc.get_page_content(page_id).unwrap()
    }
}
