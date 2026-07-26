// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

//! Freehand strokes as standard PDF ink annotations (ISO 32000,
//! annotation subtype "Ink"). Every stroke is its own annotation object
//! carrying the path coordinates (/InkList), colour (/C) and width (/BS)
//! – stored separately from the page content, which stays untouched. Any
//! standard PDF editor can therefore delete or move individual strokes
//! later. A pre-rendered appearance stream (/AP) preserves the
//! pressure-dependent line width in every viewer.
//!
//! frack itself does not rely on the appearance stream: [`load_and_strip`]
//! reads every ink annotation back into an in-memory [`Stroke`] list (the
//! single source of truth while a document is open) and removes them from
//! the copy Poppler renders, so frack draws them as an overlay it fully
//! controls (undo, erase, …). [`save_changes`] writes the list back:
//! newly drawn strokes are appended, a page is only rewritten from scratch
//! when a stroke was removed from it.

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use std::collections::{BTreeMap, HashSet};
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
    /// Not persisted per point; strokes read back from a file get 0.5.
    pub pressure: f64,
}

#[derive(Clone, Debug)]
pub struct Stroke {
    pub points: Vec<StrokePoint>,
    /// Stroke colour (RGB, 0..=1). Read from the annotation's /C so a
    /// stroke drawn elsewhere keeps its own colour instead of frack's pen.
    pub color: (f64, f64, f64),
    /// Base line width in PDF points (before the pressure factor), from /BS.
    pub width: f64,
}

/// Strokes keyed by 0-based page index.
pub type StrokesByPage = BTreeMap<usize, Vec<Stroke>>;

/// One page's worth of changes to persist.
pub enum PageChange {
    /// Add these strokes as new annotations, leaving existing ones alone.
    Append(Vec<Stroke>),
    /// Replace the page's ink annotations with exactly this list (used
    /// after a stroke was removed on the page).
    Rewrite(Vec<Stroke>),
}

/// Loads `path`, reads every ink annotation into strokes (keyed by 0-based
/// page index) and returns the document *without* those annotations,
/// serialised to bytes for Poppler to render. Non-ink annotations (links,
/// form fields, highlights, …) are left in place. frack draws the returned
/// strokes itself, so nothing is rendered twice.
pub fn load_and_strip(path: &Path) -> Result<(Vec<u8>, StrokesByPage), Box<dyn Error>> {
    let bytes = std::fs::read(path)?;
    let mut doc = Document::load_mem(&bytes)?;
    let pages: Vec<(u32, ObjectId)> = doc.get_pages().into_iter().collect();

    let mut strokes_by_page: StrokesByPage = BTreeMap::new();
    for (num, page_id) in pages {
        let page_idx = (num - 1) as usize;
        let bbox = page_box(&doc, page_id)?;
        let rotate = page_rotate(&doc, page_id);

        let mut ink_ids = Vec::new();
        let mut strokes = Vec::new();
        for aid in annot_refs(&doc, page_id) {
            if !is_ink(&doc, aid) {
                continue;
            }
            if let Ok(dict) = doc.get_dictionary(aid) {
                strokes.extend(parse_ink(&doc, dict, bbox, rotate));
            }
            ink_ids.push(aid);
        }
        if !strokes.is_empty() {
            strokes_by_page.insert(page_idx, strokes);
        }
        // Take the ink annotations out of the rendered copy.
        strip_annots(&mut doc, page_id, &ink_ids)?;
        for id in ink_ids {
            delete_annot(&mut doc, id);
        }
    }

    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok((out, strokes_by_page))
}

/// Applies per-page changes to the file. Writes atomically (temp file +
/// rename). A no-op when `changes` is empty.
pub fn save_changes(
    path: &Path,
    changes: &BTreeMap<usize, PageChange>,
) -> Result<(), Box<dyn Error>> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut doc = Document::load(path)?;
    let pages = doc.get_pages();

    for (&page_idx, change) in changes {
        let page_id = *pages
            .get(&((page_idx + 1) as u32))
            .ok_or_else(|| format!("page {} not found", page_idx + 1))?;
        let bbox = page_box(&doc, page_id)?;
        let rotate = page_rotate(&doc, page_id);

        let strokes = match change {
            PageChange::Rewrite(strokes) => {
                // Drop the page's existing ink annotations first, then
                // write the list afresh.
                let ink: Vec<ObjectId> = annot_refs(&doc, page_id)
                    .into_iter()
                    .filter(|id| is_ink(&doc, *id))
                    .collect();
                strip_annots(&mut doc, page_id, &ink)?;
                for id in ink {
                    delete_annot(&mut doc, id);
                }
                strokes
            }
            PageChange::Append(strokes) => strokes,
        };

        let annot_ids: Vec<ObjectId> = strokes
            .iter()
            .filter(|s| !s.points.is_empty())
            .map(|s| add_ink_annotation(&mut doc, s, bbox, rotate))
            .collect();
        append_annots(&mut doc, page_id, annot_ids)?;
    }

    // Atomic save: temp file in the same directory, then rename.
    let tmp = path.with_extension("pdf.frack-tmp");
    doc.save(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Creates one /Ink annotation (plus its appearance form XObject) for a
/// single stroke and returns its object id. The caller adds the id to the
/// page's /Annots array.
fn add_ink_annotation(
    doc: &mut Document,
    stroke: &Stroke,
    bbox: [f64; 4],
    rotate: i64,
) -> ObjectId {
    let color = stroke.color;
    let base_width = stroke.width;
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

    // Appearance stream: reproduces the pressure-dependent widths exactly
    // as drawn on screen. With /BBox equal to /Rect and no /Matrix the
    // form's coordinates are page coordinates. Editors that regenerate the
    // appearance fall back to /InkList and the /BS width.
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

/// Appends annotation references to the page's /Annots array (which may be
/// missing, inline, or an indirect reference).
fn append_annots(
    doc: &mut Document,
    page_id: ObjectId,
    ids: Vec<ObjectId>,
) -> Result<(), Box<dyn Error>> {
    if ids.is_empty() {
        return Ok(());
    }
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

/// Removes the given annotation references from the page's /Annots array.
/// Leaves the referenced objects in place (see [`delete_annot`]).
fn strip_annots(
    doc: &mut Document,
    page_id: ObjectId,
    remove: &[ObjectId],
) -> Result<(), Box<dyn Error>> {
    if remove.is_empty() {
        return Ok(());
    }
    let set: HashSet<ObjectId> = remove.iter().copied().collect();
    let keep = |o: &Object| o.as_reference().map(|id| !set.contains(&id)).unwrap_or(true);
    match doc.get_dictionary(page_id)?.get(b"Annots").ok().cloned() {
        Some(Object::Reference(r)) => {
            let arr = doc.get_object_mut(r).and_then(Object::as_array_mut)?;
            arr.retain(keep);
        }
        Some(Object::Array(mut v)) => {
            v.retain(keep);
            set_annots(doc, page_id, v)?;
        }
        _ => {}
    }
    Ok(())
}

/// Deletes an annotation object and its appearance XObject from the
/// document (call after [`strip_annots`] has dropped the /Annots reference).
fn delete_annot(doc: &mut Document, id: ObjectId) {
    let ap_n = doc.get_dictionary(id).ok().and_then(|d| {
        d.get(b"AP")
            .ok()
            .and_then(|ap| ap.as_dict().ok())
            .and_then(|ap| ap.get(b"N").ok())
            .and_then(|n| n.as_reference().ok())
    });
    if let Some(n) = ap_n {
        doc.objects.remove(&n);
    }
    doc.objects.remove(&id);
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

/// The object ids referenced by a page's /Annots array (inline or indirect).
fn annot_refs(doc: &Document, page_id: ObjectId) -> Vec<ObjectId> {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return Vec::new();
    };
    let arr = match page.get(b"Annots") {
        Ok(Object::Reference(r)) => doc.get_object(*r).ok().and_then(|o| o.as_array().ok()),
        Ok(Object::Array(v)) => Some(v),
        _ => None,
    };
    arr.map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
        .unwrap_or_default()
}

fn is_ink(doc: &Document, id: ObjectId) -> bool {
    doc.get_dictionary(id)
        .ok()
        .and_then(|d| d.get(b"Subtype").ok())
        .and_then(|o| o.as_name().ok())
        == Some(b"Ink".as_ref())
}

/// Parses one ink annotation into strokes (one per /InkList path, all
/// sharing the annotation's colour and width).
fn parse_ink(
    doc: &Document,
    dict: &Dictionary,
    bbox: [f64; 4],
    rotate: i64,
) -> Vec<Stroke> {
    let color = read_color(doc, dict);
    let width = read_width(doc, dict);
    let Ok(list) = dict.get(b"InkList").and_then(|o| o.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for path in list {
        let Ok(arr) = path.as_array() else { continue };
        let mut points = Vec::new();
        let mut it = arr.iter();
        while let (Some(xo), Some(yo)) = (it.next(), it.next()) {
            if let (Some(x), Some(y)) = (as_f64(doc, xo), as_f64(doc, yo)) {
                let (dx, dy) = pdf_to_display(x, y, bbox, rotate);
                points.push(StrokePoint { x: dx, y: dy, pressure: 0.5 });
            }
        }
        if !points.is_empty() {
            out.push(Stroke { points, color, width });
        }
    }
    out
}

/// Reads /C as an RGB triple. Grey (1 component) is expanded, CMYK (4) is
/// converted; anything else (incl. missing) defaults to black.
fn read_color(doc: &Document, dict: &Dictionary) -> (f64, f64, f64) {
    let Ok(c) = dict.get(b"C").and_then(|o| o.as_array()) else {
        return (0.0, 0.0, 0.0);
    };
    let v: Vec<f64> = c.iter().filter_map(|o| as_f64(doc, o)).collect();
    match v.len() {
        1 => (v[0], v[0], v[0]),
        3 => (v[0], v[1], v[2]),
        4 => {
            let (cy, m, y, k) = (v[0], v[1], v[2], v[3]);
            (
                (1.0 - cy) * (1.0 - k),
                (1.0 - m) * (1.0 - k),
                (1.0 - y) * (1.0 - k),
            )
        }
        _ => (0.0, 0.0, 0.0),
    }
}

/// Reads the border width from /BS /W (default 1.0, the PDF default).
fn read_width(doc: &Document, dict: &Dictionary) -> f64 {
    let bs = match dict.get(b"BS") {
        Ok(Object::Dictionary(d)) => Some(d.clone()),
        Ok(Object::Reference(r)) => doc.get_dictionary(*r).ok().cloned(),
        _ => None,
    };
    bs.and_then(|bs| bs.get(b"W").ok().and_then(|w| as_f64(doc, w)))
        .filter(|w| *w > 0.0)
        .unwrap_or(1.0)
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
/// top left, y down) into the page's PDF user space (origin bottom left, y
/// up, relative to the crop box).
pub fn display_to_pdf(x: f64, y: f64, bbox: [f64; 4], rotate: i64) -> (f64, f64) {
    let [x0, _y0, x1, y1] = bbox;
    let w = x1 - x0;
    let h = y1 - bbox[1];
    // (a, b): point in unrotated page coordinates, top left, y down.
    let (a, b) = match rotate.rem_euclid(360) {
        90 => (y, h - x),
        180 => (w - x, h - y),
        270 => (w - y, x),
        _ => (x, y),
    };
    (x0 + a, y1 - b)
}

/// Inverse of [`display_to_pdf`]: PDF user space → display coordinates.
pub fn pdf_to_display(u: f64, v: f64, bbox: [f64; 4], rotate: i64) -> (f64, f64) {
    let [x0, _y0, x1, y1] = bbox;
    let w = x1 - x0;
    let h = y1 - bbox[1];
    let a = u - x0;
    let b = y1 - v;
    match rotate.rem_euclid(360) {
        90 => (h - b, a),
        180 => (w - a, h - b),
        270 => (b, w - a),
        _ => (a, b),
    }
}

/// CropBox (if present, else MediaBox) – including inheritance through the
/// page tree. Poppler displays the crop box, so we map onto it.
fn page_box(doc: &Document, page_id: ObjectId) -> Result<[f64; 4], Box<dyn Error>> {
    let obj = inherited(doc, page_id, b"CropBox")
        .or_else(|| inherited(doc, page_id, b"MediaBox"))
        .ok_or("page has neither CropBox nor MediaBox")?;
    let arr = obj.as_array().map_err(|_| "box is not an array")?;
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

    fn rgb() -> (f64, f64, f64) {
        (0.8, 0.0, 0.0)
    }

    fn stroke(points: Vec<StrokePoint>) -> Stroke {
        Stroke { points, color: rgb(), width: 1.5 }
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

    fn test_strokes() -> BTreeMap<usize, PageChange> {
        let mut changes = BTreeMap::new();
        changes.insert(
            0usize,
            PageChange::Append(vec![
                stroke(vec![
                    StrokePoint { x: 50.0, y: 50.0, pressure: 0.5 },
                    StrokePoint { x: 150.0, y: 80.0, pressure: 0.7 },
                    StrokePoint { x: 250.0, y: 60.0, pressure: 0.4 },
                ]),
                stroke(vec![StrokePoint { x: 300.0, y: 400.0, pressure: 0.9 }]),
            ]),
        );
        changes
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("frack-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_creates_ink_annotations_and_leaves_content_alone() {
        let dir = tmp_dir("annot");
        let path = dir.join("test.pdf");
        make_test_pdf(&path);

        save_changes(&path, &test_strokes()).unwrap();

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

        // Appending again adds to the existing /Annots array.
        save_changes(&path, &test_strokes()).unwrap();
        let doc = Document::load(&path).unwrap();
        let dict = doc.get_dictionary(page_id).unwrap();
        assert_eq!(dict.get(b"Annots").unwrap().as_array().unwrap().len(), 4);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Strokes written to a file can be read back into the in-memory model
    /// – with their colour, width and geometry preserved – and are removed
    /// from the stripped copy Poppler renders.
    #[test]
    fn strokes_round_trip_through_the_file() {
        let dir = tmp_dir("rt");
        let path = dir.join("test.pdf");
        make_test_pdf(&path);
        save_changes(&path, &test_strokes()).unwrap();

        let (stripped, by_page) = load_and_strip(&path).unwrap();

        // Two strokes on page 0, colour and width preserved (colours are
        // stored as f32 in the PDF, so compare with a tolerance).
        let strokes = &by_page[&0];
        assert_eq!(strokes.len(), 2);
        let (r, g, b) = strokes[0].color;
        assert!((r - 0.8).abs() < 1e-6 && g.abs() < 1e-6 && b.abs() < 1e-6, "colour {:?}", strokes[0].color);
        assert!((strokes[0].width - 1.5).abs() < 1e-6);
        assert_eq!(strokes[0].points.len(), 3);
        // First point maps back to roughly where it was drawn.
        assert!((strokes[0].points[0].x - 50.0).abs() < 1e-3);
        assert!((strokes[0].points[0].y - 50.0).abs() < 1e-3);

        // The stripped copy has no ink annotations left.
        let doc = Document::load_mem(&stripped).unwrap();
        let page_id = *doc.get_pages().get(&1).unwrap();
        let has_ink = doc
            .get_dictionary(page_id)
            .ok()
            .and_then(|d| d.get(b"Annots").ok())
            .and_then(|a| a.as_array().ok())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        assert!(!has_ink, "stripped copy still has annotations");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A Rewrite replaces the page's ink annotations with exactly the given
    /// list; an empty Rewrite clears the page.
    #[test]
    fn rewrite_replaces_page_ink() {
        let dir = tmp_dir("rw");
        let path = dir.join("test.pdf");
        make_test_pdf(&path);
        save_changes(&path, &test_strokes()).unwrap();

        // Rewrite page 0 with a single stroke.
        let mut changes = BTreeMap::new();
        changes.insert(
            0usize,
            PageChange::Rewrite(vec![stroke(vec![
                StrokePoint { x: 10.0, y: 10.0, pressure: 0.5 },
                StrokePoint { x: 20.0, y: 20.0, pressure: 0.5 },
            ])]),
        );
        save_changes(&path, &changes).unwrap();

        let (_, by_page) = load_and_strip(&path).unwrap();
        assert_eq!(by_page[&0].len(), 1);

        // Empty rewrite clears the page.
        let mut changes = BTreeMap::new();
        changes.insert(0usize, PageChange::Rewrite(Vec::new()));
        save_changes(&path, &changes).unwrap();
        let (_, by_page) = load_and_strip(&path).unwrap();
        assert!(!by_page.contains_key(&0), "page still has strokes: {by_page:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Deleting the annotations (as any external PDF editor would) restores
    /// a page that is structurally identical to the original – the strokes
    /// leave no trace in the content stream.
    #[test]
    fn annotations_are_deletable() {
        let dir = tmp_dir("del");
        let path = dir.join("test.pdf");
        make_test_pdf(&path);
        let original_content = page_content(&path);

        save_changes(&path, &test_strokes()).unwrap();

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
