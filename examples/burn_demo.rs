// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

//! Creates a test PDF (2 pages, page 2 with /Rotate 90), burns some
//! strokes into it and saves the result. Render with `pdftoppm` for a
//! visual check. Usage: burn_demo <output directory>

use lopdf::{dictionary, Document, Object, Stream};
use frack::burn::{burn_strokes, Stroke, StrokePoint};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn staff_lines_content() -> Vec<u8> {
    // Five "staff lines" across the (unrotated) page.
    let mut ops = String::from("0 0 0 RG\n1 w\n");
    for i in 0..5 {
        let y = 700 - i * 12;
        ops.push_str(&format!("60 {y} m 552 {y} l S\n"));
    }
    ops.into_bytes()
}

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("Aufruf: burn_demo <ausgabeverzeichnis>"),
    );
    std::fs::create_dir_all(&dir).unwrap();
    let pdf_path = dir.join("demo.pdf");

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let c1 = doc.add_object(Stream::new(dictionary! {}, staff_lines_content()));
    let p1 = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => c1,
    });
    let c2 = doc.add_object(Stream::new(dictionary! {}, staff_lines_content()));
    let p2 = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => c2,
        "Rotate" => 90,
    });
    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => vec![p1.into(), p2.into()],
        "Count" => 2,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.save(&pdf_path).unwrap();

    // Strokes in display coordinates (origin top left, y down).
    let mut strokes = BTreeMap::new();
    // Page 1 (unrotated): an "X" over the staff lines (which sit at PDF
    // y 652..700, i.e. display y 92..140) plus a pressure wave.
    strokes.insert(
        0usize,
        vec![
            Stroke {
                points: vec![
                    StrokePoint { x: 100.0, y: 80.0, pressure: 0.6 },
                    StrokePoint { x: 200.0, y: 160.0, pressure: 0.6 },
                ],
            },
            Stroke {
                points: vec![
                    StrokePoint { x: 200.0, y: 80.0, pressure: 0.6 },
                    StrokePoint { x: 100.0, y: 160.0, pressure: 0.6 },
                ],
            },
            Stroke {
                points: (0..=60)
                    .map(|i| {
                        let t = i as f64 / 60.0;
                        StrokePoint {
                            x: 80.0 + 440.0 * t,
                            y: 250.0 + 30.0 * (t * 12.0).sin(),
                            pressure: 0.2 + 0.8 * t,
                        }
                    })
                    .collect(),
            },
        ],
    );
    // Page 2 (/Rotate 90, display 792x612): horizontal line near the top
    // display edge – must appear at the top after rendering.
    strokes.insert(
        1usize,
        vec![Stroke {
            points: vec![
                StrokePoint { x: 50.0, y: 40.0, pressure: 0.6 },
                StrokePoint { x: 742.0, y: 40.0, pressure: 0.6 },
            ],
        }],
    );

    burn_strokes(&pdf_path, &strokes, (0.8, 0.0, 0.0), 2.0).unwrap();
    println!("geschrieben: {}", pdf_path.display());
}
