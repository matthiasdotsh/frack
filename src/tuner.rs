// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

//! Tuner bar: next to the usual instantaneous readout, it shows a
//! scrolling history of the pitch deviation in cents over the last few
//! seconds, so intonation trends across a phrase stay visible.
//!
//! Audio flows from a cpal capture thread through a ring buffer into a
//! periodic analysis tick on the main loop (McLeod pitch detector).

use crate::config::Config;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use gtk::cairo;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use pitch_detection::detector::mcleod::McLeodDetector;
use pitch_detection::detector::PitchDetector;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Analysis interval; also the x resolution of the history graph.
const TICK_MS: u64 = 50;
/// Length of the visible history.
const HISTORY_SECS: f64 = 8.0;
/// Analysis window (~85 ms at 48 kHz; enough for low string notes).
const WINDOW: usize = 4096;
/// RMS below this counts as silence (no reading).
const SILENCE_RMS: f32 = 0.005;
/// Minimum clarity (McLeod) to accept a pitch.
const MIN_CLARITY: f64 = 0.6;
/// Accepted frequency range (below piano A0 / above C8 is noise).
const FREQ_RANGE: std::ops::Range<f64> = 25.0..4500.0;

const BAR_HEIGHT: i32 = 110;
/// Width of the info panel (note name, cents, Hz) left of the graph.
const PANEL_W: f64 = 130.0;
/// Y axis range of the graph in cents.
const CENTS_RANGE: f64 = 50.0;

#[derive(Clone, Copy, PartialEq)]
pub struct Reading {
    /// MIDI note number of the nearest note.
    pub midi: i32,
    /// Deviation from that note in cents (-50..=50).
    pub cents: f64,
    pub freq: f64,
}

pub struct TunerInner {
    pub widget: gtk::DrawingArea,
    cfg: Rc<Config>,
    history: RefCell<VecDeque<Option<Reading>>>,
    ring: Arc<Mutex<VecDeque<f32>>>,
    stream: RefCell<Option<cpal::Stream>>,
    tick: RefCell<Option<glib::SourceId>>,
    sample_rate: Cell<u32>,
    detector: RefCell<Option<McLeodDetector<f64>>>,
}

#[derive(Clone)]
pub struct Tuner(Rc<TunerInner>);

impl Tuner {
    pub fn new(cfg: Rc<Config>) -> Self {
        let widget = gtk::DrawingArea::new();
        widget.set_content_height(BAR_HEIGHT);
        widget.set_hexpand(true);
        widget.set_visible(false);
        let tuner = Tuner(Rc::new(TunerInner {
            widget,
            cfg,
            history: RefCell::new(VecDeque::new()),
            ring: Arc::new(Mutex::new(VecDeque::new())),
            stream: RefCell::new(None),
            tick: RefCell::new(None),
            sample_rate: Cell::new(48000),
            detector: RefCell::new(None),
        }));
        tuner.setup_draw();
        tuner
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.0.widget
    }

    /// Opens the default input device and starts the analysis tick.
    pub fn start(&self) -> Result<(), String> {
        if self.0.stream.borrow().is_some() {
            return Ok(());
        }
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("no input device found")?;
        let supported = device
            .default_input_config()
            .map_err(|e| format!("input format: {e}"))?;
        let sample_format = supported.sample_format();
        let stream_cfg: cpal::StreamConfig = supported.into();
        let channels = stream_cfg.channels as usize;
        self.0.sample_rate.set(stream_cfg.sample_rate);
        self.0.detector.replace(None);
        self.0.ring.lock().unwrap().clear();

        let err_fn = |e| eprintln!("audio error: {e}");
        let r_f32 = self.0.ring.clone();
        let r_i16 = self.0.ring.clone();
        let r_u16 = self.0.ring.clone();
        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                stream_cfg,
                move |data: &[f32], _: &_| push_frames(&r_f32, data, channels),
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                stream_cfg,
                move |data: &[i16], _: &_| {
                    let f: Vec<f32> =
                        data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                    push_frames(&r_i16, &f, channels);
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_input_stream(
                stream_cfg,
                move |data: &[u16], _: &_| {
                    let f: Vec<f32> = data
                        .iter()
                        .map(|&s| (s as f32 - 32768.0) / 32768.0)
                        .collect();
                    push_frames(&r_u16, &f, channels);
                },
                err_fn,
                None,
            ),
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| format!("input stream: {e}"))?;
        stream.play().map_err(|e| format!("starting capture: {e}"))?;
        *self.0.stream.borrow_mut() = Some(stream);

        let t = self.clone();
        let id = glib::timeout_add_local(std::time::Duration::from_millis(TICK_MS), move || {
            t.analyze_tick();
            glib::ControlFlow::Continue
        });
        *self.0.tick.borrow_mut() = Some(id);
        self.0.widget.set_visible(true);
        Ok(())
    }

    pub fn stop(&self) {
        *self.0.stream.borrow_mut() = None;
        if let Some(id) = self.0.tick.borrow_mut().take() {
            id.remove();
        }
        self.0.history.borrow_mut().clear();
        self.0.widget.set_visible(false);
    }

    fn analyze_tick(&self) {
        let samples: Option<Vec<f64>> = {
            let ring = self.0.ring.lock().unwrap();
            (ring.len() >= WINDOW).then(|| {
                ring.iter()
                    .skip(ring.len() - WINDOW)
                    .map(|&s| s as f64)
                    .collect()
            })
        };
        let reading = samples.and_then(|s| {
            detect(
                &mut self.0.detector.borrow_mut(),
                &s,
                self.0.sample_rate.get() as usize,
            )
        });
        let reading = reading.and_then(|freq| freq_to_reading(freq, self.0.cfg.a4));

        let mut hist = self.0.history.borrow_mut();
        let cap = (HISTORY_SECS * 1000.0 / TICK_MS as f64) as usize;
        while hist.len() >= cap {
            hist.pop_front();
        }
        hist.push_back(reading);
        drop(hist);
        self.0.widget.queue_draw();
    }

    fn setup_draw(&self) {
        let t = self.clone();
        self.0.widget.set_draw_func(move |_, cr, w, h| {
            draw_tuner(
                cr,
                w as f64,
                h as f64,
                &t.0.history.borrow(),
                &t.0.cfg,
            );
        });
    }
}

/// Downmixes interleaved frames to mono and appends them to the ring.
fn push_frames(ring: &Arc<Mutex<VecDeque<f32>>>, data: &[f32], channels: usize) {
    let mut ring = ring.lock().unwrap();
    for frame in data.chunks_exact(channels.max(1)) {
        let mono = frame.iter().sum::<f32>() / frame.len() as f32;
        if ring.len() >= WINDOW * 2 {
            ring.pop_front();
        }
        ring.push_back(mono);
    }
}

/// Runs the pitch detector over one window; None for silence/noise.
pub fn detect(
    detector: &mut Option<McLeodDetector<f64>>,
    samples: &[f64],
    sample_rate: usize,
) -> Option<f64> {
    let rms = (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).sqrt();
    if rms < SILENCE_RMS as f64 {
        return None;
    }
    let det = detector.get_or_insert_with(|| McLeodDetector::new(WINDOW, WINDOW / 2));
    let pitch = det.get_pitch(samples, sample_rate, 0.0, MIN_CLARITY)?;
    FREQ_RANGE.contains(&pitch.frequency).then_some(pitch.frequency)
}

/// Nearest note and deviation for a frequency, relative to the given A4.
pub fn freq_to_reading(freq: f64, a4: f64) -> Option<Reading> {
    if freq <= 0.0 || a4 <= 0.0 {
        return None;
    }
    let midi_f = 69.0 + 12.0 * (freq / a4).log2();
    let midi = midi_f.round() as i32;
    if !(12..=127).contains(&midi) {
        return None;
    }
    let cents = (midi_f - midi as f64) * 100.0;
    Some(Reading { midi, cents, freq })
}

/// Note name for a MIDI number. `german` uses H for B natural and B for
/// Bb; `flat` prefers flat spellings (Eb/Es) over sharps (D#/Dis).
pub fn note_name(midi: i32, german: bool, flat: bool) -> String {
    const EN_SHARP: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    const EN_FLAT: [&str; 12] = [
        "C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B",
    ];
    const DE_SHARP: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "B", "H",
    ];
    const DE_FLAT: [&str; 12] = [
        "C", "Des", "D", "Es", "E", "F", "Ges", "G", "As", "A", "B", "H",
    ];
    let names = match (german, flat) {
        (true, true) => DE_FLAT,
        (true, false) => DE_SHARP,
        (false, true) => EN_FLAT,
        (false, false) => EN_SHARP,
    };
    let octave = midi / 12 - 1;
    format!("{}{}", names[midi.rem_euclid(12) as usize], octave)
}

/// Color for a deviation: green when in tune, red when far off.
fn cents_color(cents: f64) -> (f64, f64, f64) {
    let c = cents.abs();
    if c < 5.0 {
        (0.30, 0.85, 0.35)
    } else if c < 15.0 {
        (0.85, 0.80, 0.25)
    } else if c < 30.0 {
        (0.95, 0.55, 0.20)
    } else {
        (0.95, 0.30, 0.25)
    }
}

fn draw_tuner(
    cr: &cairo::Context,
    w: f64,
    h: f64,
    history: &VecDeque<Option<Reading>>,
    cfg: &Config,
) {
    let german = cfg.note_names.eq_ignore_ascii_case("german");
    let flat = cfg.accidentals.eq_ignore_ascii_case("flat");
    cr.set_source_rgb(0.08, 0.08, 0.10);
    let _ = cr.paint();

    // --- Graph area ---
    let gx = PANEL_W;
    let gw = (w - PANEL_W).max(1.0);
    let mid = h / 2.0;
    let cents_to_y = |cents: f64| mid - (cents / CENTS_RANGE) * (h / 2.0 - 8.0);

    // Grid: center line = in tune, faint lines at ±25 cents.
    cr.set_line_width(1.0);
    cr.set_source_rgb(0.25, 0.25, 0.28);
    for c in [-25.0, 25.0] {
        cr.move_to(gx, cents_to_y(c));
        cr.line_to(w, cents_to_y(c));
        let _ = cr.stroke();
    }
    cr.set_source_rgb(0.45, 0.75, 0.50);
    cr.move_to(gx, mid);
    cr.line_to(w, mid);
    let _ = cr.stroke();

    // History curve: right edge = now. Segments connect consecutive
    // readings of the same note; note changes get a small label.
    let cap = (HISTORY_SECS * 1000.0 / TICK_MS as f64) as usize;
    let dx = gw / (cap.max(2) - 1) as f64;
    let x_of = |i: usize| w - (history.len().saturating_sub(1 + i)) as f64 * dx;
    cr.set_line_width(2.0);
    let mut prev: Option<(usize, Reading)> = None;
    for (i, item) in history.iter().enumerate() {
        let Some(r) = item else {
            prev = None;
            continue;
        };
        if let Some((pi, pr)) = prev
            && pr.midi == r.midi
            && i - pi == 1
        {
            let (cr_, cg, cb) = cents_color((pr.cents + r.cents) / 2.0);
            cr.set_source_rgb(cr_, cg, cb);
            cr.move_to(x_of(pi), cents_to_y(pr.cents));
            cr.line_to(x_of(i), cents_to_y(r.cents));
            let _ = cr.stroke();
        }
        // Label the start of a new note directly at the curve.
        if prev.map(|(_, pr)| pr.midi != r.midi).unwrap_or(true) {
            cr.set_source_rgb(0.75, 0.75, 0.80);
            cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
            cr.set_font_size(11.0);
            cr.move_to(x_of(i) + 2.0, (cents_to_y(r.cents) - 6.0).max(12.0));
            let _ = cr.show_text(&note_name(r.midi, german, flat));
        }
        prev = Some((i, *r));
    }

    // --- Info panel (current reading) ---
    let last = history.iter().rev().flatten().next();
    cr.set_source_rgb(0.12, 0.12, 0.15);
    cr.rectangle(0.0, 0.0, PANEL_W, h);
    let _ = cr.fill();
    match last {
        Some(r) => {
            let (cr_, cg, cb) = cents_color(r.cents);
            cr.set_source_rgb(cr_, cg, cb);
            cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
            cr.set_font_size(38.0);
            cr.move_to(10.0, h / 2.0 + 8.0);
            let _ = cr.show_text(&note_name(r.midi, german, flat));
            cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
            cr.set_font_size(13.0);
            cr.move_to(10.0, h - 28.0);
            let _ = cr.show_text(&format!("{:+.0} Cent", r.cents));
            cr.set_source_rgb(0.6, 0.6, 0.65);
            cr.move_to(10.0, h - 10.0);
            let _ = cr.show_text(&format!("{:.1} Hz", r.freq));
        }
        None => {
            cr.set_source_rgb(0.5, 0.5, 0.55);
            cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
            cr.set_font_size(14.0);
            cr.move_to(10.0, h / 2.0 + 5.0);
            let _ = cr.show_text("…");
        }
    }
    // A4 reference in the corner.
    cr.set_source_rgb(0.45, 0.45, 0.50);
    cr.set_font_size(11.0);
    cr.move_to(10.0, 16.0);
    let _ = cr.show_text(&format!("a¹ = {:.0} Hz", cfg.a4));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_math() {
        let r = freq_to_reading(440.0, 440.0).unwrap();
        assert_eq!(r.midi, 69);
        assert!(r.cents.abs() < 1e-9);
        assert_eq!(note_name(r.midi, false, false), "A4");

        // 10 cents sharp of A4.
        let f = 440.0 * 2f64.powf(10.0 / 1200.0);
        let r = freq_to_reading(f, 440.0).unwrap();
        assert_eq!(r.midi, 69);
        assert!((r.cents - 10.0).abs() < 1e-6);

        // Reference pitch shifts the mapping: 442 Hz is A4 at a4=442.
        let r = freq_to_reading(442.0, 442.0).unwrap();
        assert_eq!(r.midi, 69);
        assert!(r.cents.abs() < 1e-9);

        // German names: B natural is H, Bb is B.
        assert_eq!(note_name(71, true, false), "H4");
        assert_eq!(note_name(70, true, false), "B4");
        assert_eq!(note_name(71, false, false), "B4");
        assert_eq!(note_name(60, true, false), "C4");

        // Flat spellings: Es instead of Dis, Eb instead of D#.
        assert_eq!(note_name(63, true, false), "D#4");
        assert_eq!(note_name(63, true, true), "Es4");
        assert_eq!(note_name(63, false, true), "Eb4");
        assert_eq!(note_name(61, true, true), "Des4");
        assert_eq!(note_name(70, true, true), "B4");
        assert_eq!(note_name(71, true, true), "H4");
    }

    #[test]
    fn detects_synthesized_sine() {
        let rate = 48000usize;
        for freq in [110.0f64, 261.63, 440.0, 880.0] {
            let samples: Vec<f64> = (0..WINDOW)
                .map(|i| 0.3 * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin())
                .collect();
            let mut det = None;
            let got = detect(&mut det, &samples, rate).expect("no pitch detected");
            assert!(
                (got - freq).abs() < freq * 0.01,
                "expected {freq} Hz, got {got} Hz"
            );
        }
    }

    #[test]
    fn silence_gives_no_reading() {
        let samples = vec![0.0f64; WINDOW];
        let mut det = None;
        assert!(detect(&mut det, &samples, 48000).is_none());
    }
}
