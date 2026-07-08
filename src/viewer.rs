// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

//! Displays a PDF with half-page turns (the next page's top half appears
//! first) and freehand stylus annotations. Annotations are kept in memory
//! until the page is turned or the document is closed, then burned into
//! the file (see burn.rs).
//!
//! Rendered pages are cached as bitmaps and neighboring pages are
//! pre-rendered while idle – turning a page only copies pixels.

use crate::burn::{self, Stroke, StrokePoint};
use crate::config::Config;
use gtk::cairo;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Display position. `Split(n)` shows the top half of page n+1 above the
/// bottom half of page n – you finish playing page n while the beginning
/// of the next page is already visible.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ViewPos {
    Full(usize),
    Split(usize),
}

impl ViewPos {
    /// The page currently being played (the bottom/current one in Split).
    fn base_page(self) -> usize {
        match self {
            ViewPos::Full(n) | ViewPos::Split(n) => n,
        }
    }
}

/// A page rendered at device-pixel resolution (page scale × display factor).
struct CachedPage {
    s_px: f64,
    surface: cairo::ImageSurface,
}

/// A crisp render of the zoomed viewport (only used while zoom > 1).
struct ZoomSurface {
    zoom: f64,
    ox: f64,
    oy: f64,
    w_px: i32,
    h_px: i32,
    sf: f64,
    surface: cairo::ImageSurface,
}

/// Transform state at the start of a pinch gesture.
struct PinchStart {
    zoom: f64,
    ox: f64,
    oy: f64,
    cx: f64,
    cy: f64,
}

/// A rendered thumbnail as raw pixels: page, width, height, stride, data.
type ThumbPixels = (usize, i32, i32, i32, Vec<u8>);

/// Queue and results shared with the thumbnail worker thread. Rendering
/// happens off the main thread because a single scanned page can take
/// hundreds of milliseconds and would freeze the UI.
struct ThumbState {
    queue: VecDeque<usize>,
    /// Pages already claimed by a worker (avoids duplicate renders).
    taken: HashSet<usize>,
    results: Vec<ThumbPixels>,
    quit: bool,
    done: bool,
    /// Number of worker threads still running.
    active: usize,
}

struct ThumbWorker {
    shared: Arc<Mutex<ThumbState>>,
    timer: glib::SourceId,
}

const MAX_ZOOM: f64 = 6.0;

/// Render height of slider preview thumbnails (pixels).
const THUMB_H: f64 = 240.0;
/// Content size of the preview tile above the slider.
const PREVIEW_W: i32 = 190;
const PREVIEW_H: i32 = 250;

pub struct DocState {
    pub path: PathBuf,
    pub doc: poppler::Document,
    pub n_pages: usize,
    pub pos: ViewPos,
    pub annotate: bool,
    /// Strokes not yet burned in, per 0-based page index.
    pub pending: BTreeMap<usize, Vec<Stroke>>,
    /// The stroke currently being drawn.
    pub current: Option<Stroke>,
    cache: BTreeMap<usize, CachedPage>,
    /// Small page renders for the slider preview, cached per page.
    thumbs: BTreeMap<usize, cairo::ImageSurface>,
    /// Pinch zoom factor; 1.0 = page fits the area (no zoom).
    zoom: f64,
    /// Widget-space translation of the page origin while zoomed.
    view: (f64, f64),
    zoom_cache: Option<ZoomSurface>,
}

#[derive(Clone)]
pub struct Viewer {
    pub area: gtk::DrawingArea,
    pub state: Rc<RefCell<Option<DocState>>>,
    cfg: Rc<Config>,
    status: gtk::Label,
    pen_button: gtk::ToggleButton,
    /// Some(...) while a pinch gesture is in progress.
    pinch: Rc<RefCell<Option<PinchStart>>>,
    /// True while a crisp zoom render is already scheduled.
    zoom_job: Rc<std::cell::Cell<bool>>,
    /// Overlay wrapping the drawing area plus the page slider.
    overlay: gtk::Overlay,
    nav_box: gtk::Box,
    nav_scale: gtk::Scale,
    nav_label: gtk::Label,
    nav_actions: gtk::Box,
    /// True while the slider is being updated programmatically.
    nav_updating: Rc<std::cell::Cell<bool>>,
    /// Debounce timer: jump only once the slider value settles.
    nav_timer: Rc<RefCell<Option<glib::SourceId>>>,
    preview_box: gtk::Box,
    preview_area: gtk::DrawingArea,
    preview_label: gtk::Label,
    /// 0-based page currently shown in the preview tile.
    preview_page: Rc<std::cell::Cell<usize>>,
    /// Background thumbnail renderer, running while thumbs are missing.
    thumb_worker: Rc<RefCell<Option<ThumbWorker>>>,
}

impl Viewer {
    pub fn new(cfg: Rc<Config>, status: gtk::Label, pen_button: gtk::ToggleButton) -> Self {
        let area = gtk::DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);

        // Bottom overlay, toggled by a middle tap: a row of action
        // buttons (filled by main) above the page slider. This is also
        // the touch-only escape hatch in fullscreen, where the header
        // bar is hidden.
        let nav_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 1.0, 2.0, 1.0);
        nav_scale.set_digits(0);
        nav_scale.set_hexpand(true);
        let nav_label = gtk::Label::new(None);
        nav_label.set_width_chars(9);
        let slider_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        slider_row.append(&nav_scale);
        slider_row.append(&nav_label);
        let nav_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        // Preview tile shown above the slider while scrubbing: a page
        // thumbnail plus a large page number.
        let preview_area = gtk::DrawingArea::new();
        preview_area.set_content_width(PREVIEW_W);
        preview_area.set_content_height(PREVIEW_H);
        let preview_label = gtk::Label::new(None);
        preview_label.add_css_class("title-2");
        let preview_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        preview_box.set_halign(gtk::Align::Center);
        preview_box.append(&preview_area);
        preview_box.append(&preview_label);
        preview_box.set_visible(false);
        let nav_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        nav_box.append(&preview_box);
        nav_box.add_css_class("osd");
        nav_box.add_css_class("toolbar");
        nav_box.set_valign(gtk::Align::End);
        nav_box.set_margin_start(24);
        nav_box.set_margin_end(24);
        nav_box.set_margin_bottom(16);
        nav_box.append(&nav_actions);
        nav_box.append(&slider_row);
        nav_box.set_visible(false);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&area));
        overlay.add_overlay(&nav_box);

        let viewer = Viewer {
            area,
            state: Rc::new(RefCell::new(None)),
            cfg,
            status,
            pen_button,
            pinch: Rc::new(RefCell::new(None)),
            zoom_job: Rc::new(std::cell::Cell::new(false)),
            overlay,
            nav_box,
            nav_scale,
            nav_label,
            nav_actions,
            nav_updating: Rc::new(std::cell::Cell::new(false)),
            nav_timer: Rc::new(RefCell::new(None)),
            preview_box,
            preview_area,
            preview_label,
            preview_page: Rc::new(std::cell::Cell::new(0)),
            thumb_worker: Rc::new(RefCell::new(None)),
        };
        viewer.setup_draw();
        viewer.setup_gestures();
        viewer.setup_pen_button();
        viewer.setup_nav();
        viewer.setup_preview();
        viewer
    }

    /// The widget to embed: drawing area plus slider overlay.
    pub fn widget(&self) -> &gtk::Overlay {
        &self.overlay
    }

    /// Button row in the navigation overlay; main adds actions here
    /// (back, pen, undo, tuner, fullscreen) for keyboard-free use.
    pub fn nav_actions(&self) -> &gtk::Box {
        &self.nav_actions
    }

    pub fn open(&self, path: &Path) -> Result<(), String> {
        self.close();
        let abs = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
        let doc = load_document(&abs)?;
        let n_pages = doc.n_pages().max(0) as usize;
        if n_pages == 0 {
            return Err("PDF has no pages".to_string());
        }
        *self.state.borrow_mut() = Some(DocState {
            path: abs,
            doc,
            n_pages,
            pos: ViewPos::Full(0),
            annotate: false,
            pending: BTreeMap::new(),
            current: None,
            cache: BTreeMap::new(),
            thumbs: BTreeMap::new(),
            zoom: 1.0,
            view: (0.0, 0.0),
            zoom_cache: None,
        });
        self.pen_button.set_active(false);
        self.nav_updating.set(true);
        self.nav_scale.set_range(1.0, n_pages.max(2) as f64);
        self.nav_scale.set_value(1.0);
        self.nav_updating.set(false);
        self.nav_box.set_visible(false);
        self.update_status();
        self.area.queue_draw();
        // Generate slider previews in the background right away, so they
        // are ready by the time the slider is first used.
        self.start_thumbs();
        Ok(())
    }

    /// Jumps directly to a 0-based page (full-page view).
    pub fn goto_page(&self, page: usize) {
        self.flush();
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        let page = page.min(st.n_pages - 1);
        if st.pos == ViewPos::Full(page) {
            return;
        }
        st.pos = ViewPos::Full(page);
        reset_zoom(st);
        drop(guard);
        self.update_status();
        self.area.queue_draw();
    }

    /// Shows/hides the page slider (middle tap).
    pub fn toggle_nav(&self) {
        if self.state.borrow().is_none() {
            return;
        }
        let show = !self.nav_box.is_visible();
        self.nav_box.set_visible(show);
        self.preview_box.set_visible(false);
        if show {
            self.sync_nav();
            self.start_thumbs();
        }
    }

    pub fn nav_visible(&self) -> bool {
        self.nav_box.is_visible()
    }

    pub fn hide_nav(&self) {
        self.nav_box.set_visible(false);
    }

    /// Aligns slider and label with the current position.
    fn sync_nav(&self) {
        let guard = self.state.borrow();
        let Some(st) = guard.as_ref() else { return };
        let page = st.pos.base_page() + 1;
        let n = st.n_pages;
        drop(guard);
        self.nav_updating.set(true);
        self.nav_scale.set_value(page as f64);
        self.nav_updating.set(false);
        self.nav_label.set_text(&format!("{page} / {n}"));
    }

    fn setup_nav(&self) {
        let v = self.clone();
        self.nav_scale.connect_value_changed(move |scale| {
            if v.nav_updating.get() {
                return;
            }
            let target = scale.value().round() as usize;
            let n = v
                .state
                .borrow()
                .as_ref()
                .map(|st| st.n_pages)
                .unwrap_or(1);
            let target = target.clamp(1, n);
            v.nav_label.set_text(&format!("{target} / {n}"));
            v.show_preview(target - 1, n);
            // Debounce: only jump once the value has settled briefly, so
            // dragging does not render every page in between.
            if let Some(id) = v.nav_timer.borrow_mut().take() {
                id.remove();
            }
            let v2 = v.clone();
            let id = glib::timeout_add_local_once(
                std::time::Duration::from_millis(200),
                move || {
                    v2.nav_timer.borrow_mut().take();
                    v2.preview_box.set_visible(false);
                    v2.goto_page(target - 1);
                },
            );
            *v.nav_timer.borrow_mut() = Some(id);
        });
    }

    /// Shows the preview tile for a 0-based page while scrubbing. The
    /// thumbnail is rendered by the worker; until it arrives the tile
    /// shows a placeholder.
    fn show_preview(&self, page: usize, n_pages: usize) {
        self.preview_page.set(page);
        self.preview_label.set_text(&format!("{} / {n_pages}", page + 1));
        self.preview_box.set_visible(true);
        self.prioritize_thumb(page);
        self.preview_area.queue_draw();
    }

    fn setup_preview(&self) {
        let v = self.clone();
        self.preview_area.set_draw_func(move |_, cr, w, h| {
            let w = w as f64;
            let h = h as f64;
            cr.set_source_rgb(0.10, 0.10, 0.12);
            let _ = cr.paint();
            let guard = v.state.borrow();
            let Some(st) = guard.as_ref() else { return };
            let page = v.preview_page.get();
            let Some(thumb) = st.thumbs.get(&page) else {
                // Not rendered yet: draw a light page-like placeholder
                // with the page number while the worker catches up.
                cr.set_source_rgb(0.92, 0.92, 0.90);
                cr.rectangle(w * 0.14, 6.0, w * 0.72, h - 12.0);
                let _ = cr.fill();
                cr.set_source_rgb(0.55, 0.55, 0.58);
                cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
                cr.set_font_size(32.0);
                let text = format!("{}", page + 1);
                if let Ok(ext) = cr.text_extents(&text) {
                    cr.move_to(w / 2.0 - ext.width() / 2.0, h / 2.0 + 12.0);
                    let _ = cr.show_text(&text);
                }
                return;
            };
            let (tw, th) = (thumb.width() as f64, thumb.height() as f64);
            let s = (w / tw).min(h / th);
            cr.save().ok();
            cr.translate((w - tw * s) / 2.0, (h - th * s) / 2.0);
            cr.scale(s, s);
            let _ = cr.set_source_surface(thumb, 0.0, 0.0);
            let _ = cr.paint();
            cr.restore().ok();
        });
    }

    /// Starts the thumbnail worker for all missing pages (nearest to the
    /// current page first). No-op while a worker is already running.
    fn start_thumbs(&self) {
        // A finished worker no longer serves its queue; replace it.
        let finished = self
            .thumb_worker
            .borrow()
            .as_ref()
            .map(|w| w.shared.lock().unwrap().done)
            .unwrap_or(false);
        if finished {
            self.stop_thumbs();
        } else if self.thumb_worker.borrow().is_some() {
            return;
        }
        let (path, pages) = {
            let guard = self.state.borrow();
            let Some(st) = guard.as_ref() else { return };
            let base = st.pos.base_page();
            let mut pages: Vec<usize> = (0..st.n_pages)
                .filter(|p| !st.thumbs.contains_key(p))
                .collect();
            if pages.is_empty() {
                return;
            }
            pages.sort_by_key(|p| p.abs_diff(base));
            (st.path.clone(), pages)
        };
        // Several workers, each with its own Poppler document, drain the
        // queue in parallel; scanned pages decode slowly on one core.
        let n_workers = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1))
            .unwrap_or(1)
            .clamp(1, 4);
        let shared = Arc::new(Mutex::new(ThumbState {
            queue: pages.into(),
            taken: HashSet::new(),
            results: Vec::new(),
            quit: false,
            done: false,
            active: n_workers,
        }));
        for _ in 0..n_workers {
            let shared = shared.clone();
            let path = path.clone();
            std::thread::spawn(move || thumb_worker(path, shared));
        }
        // Collect finished thumbnails on the main thread.
        let v = self.clone();
        let shared_poll = shared.clone();
        let timer = glib::timeout_add_local(std::time::Duration::from_millis(80), move || {
            let (results, done) = {
                let mut g = shared_poll.lock().unwrap();
                (std::mem::take(&mut g.results), g.done)
            };
            if !results.is_empty() {
                let mut guard = v.state.borrow_mut();
                if let Some(st) = guard.as_mut() {
                    for (page, w, h, stride, data) in results {
                        if let Ok(surf) = cairo::ImageSurface::create_for_data(
                            data,
                            cairo::Format::ARgb32,
                            w,
                            h,
                            stride,
                        ) {
                            st.thumbs.insert(page, surf);
                        }
                    }
                }
                drop(guard);
                if v.preview_box.is_visible() {
                    v.preview_area.queue_draw();
                }
            }
            if done {
                // The worker exited; drop the handle without removing the
                // timer source (returning Break destroys it).
                v.thumb_worker.borrow_mut().take();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        *self.thumb_worker.borrow_mut() = Some(ThumbWorker { shared, timer });
    }

    fn stop_thumbs(&self) {
        if let Some(worker) = self.thumb_worker.borrow_mut().take() {
            worker.shared.lock().unwrap().quit = true;
            worker.timer.remove();
        }
    }

    /// Moves a page to the front of the thumbnail queue (scrub target).
    fn prioritize_thumb(&self, page: usize) {
        let missing = self
            .state
            .borrow()
            .as_ref()
            .map(|st| !st.thumbs.contains_key(&page))
            .unwrap_or(false);
        if !missing {
            return;
        }
        self.start_thumbs();
        if let Some(worker) = self.thumb_worker.borrow().as_ref() {
            let mut g = worker.shared.lock().unwrap();
            if let Some(idx) = g.queue.iter().position(|&p| p == page) {
                g.queue.remove(idx);
            }
            g.queue.push_front(page);
        }
    }

    /// Saves pending annotations and closes the document.
    pub fn close(&self) {
        self.flush();
        self.stop_thumbs();
        *self.state.borrow_mut() = None;
        self.nav_box.set_visible(false);
        self.area.queue_draw();
    }

    /// Burns all pending strokes into the file and reloads it.
    pub fn flush(&self) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        if let Some(cur) = st.current.take() {
            st.pending.entry(st.pos.base_page()).or_default().push(cur);
        }
        if st.pending.values().all(|v| v.is_empty()) {
            st.pending.clear();
            return;
        }
        match burn::burn_strokes(&st.path, &st.pending, self.cfg.pen_rgb(), self.cfg.pen_width) {
            Ok(()) => {
                // Drop cache entries for the modified pages; the rest stays
                // valid. The thumbnail worker holds a now-stale document,
                // so stop it too; it restarts on the next slider use.
                for page in st.pending.keys() {
                    st.cache.remove(page);
                    st.thumbs.remove(page);
                }
                self.stop_thumbs();
                st.zoom_cache = None;
                st.pending.clear();
                match load_document(&st.path) {
                    Ok(doc) => st.doc = doc,
                    Err(e) => {
                        drop(guard);
                        self.status
                            .set_text(&format!("Fehler beim Neuladen: {e}"));
                        return;
                    }
                }
            }
            Err(e) => {
                drop(guard);
                self.status
                    .set_text(&format!("Fehler beim Speichern: {e}"));
                return;
            }
        }
        drop(guard);
        self.area.queue_draw();
    }

    pub fn forward(&self) {
        self.flush();
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        st.pos = match st.pos {
            // In annotation mode turn whole pages (drawing needs the full page).
            ViewPos::Full(n) if st.annotate && n + 1 < st.n_pages => ViewPos::Full(n + 1),
            ViewPos::Full(n) if !st.annotate && n + 1 < st.n_pages => ViewPos::Split(n),
            ViewPos::Split(n) => ViewPos::Full(n + 1),
            other => other,
        };
        reset_zoom(st);
        drop(guard);
        self.update_status();
        self.area.queue_draw();
    }

    pub fn backward(&self) {
        self.flush();
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        st.pos = match st.pos {
            ViewPos::Full(n) if st.annotate && n > 0 => ViewPos::Full(n - 1),
            ViewPos::Full(n) if !st.annotate && n > 0 => ViewPos::Split(n - 1),
            ViewPos::Split(n) => ViewPos::Full(n),
            other => other,
        };
        reset_zoom(st);
        drop(guard);
        self.update_status();
        self.area.queue_draw();
    }

    /// Removes the last stroke (not yet burned in) on the current page.
    pub fn undo(&self) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        if st.current.take().is_none() {
            let page = st.pos.base_page();
            if let Some(v) = st.pending.get_mut(&page) {
                v.pop();
            }
        }
        drop(guard);
        self.area.queue_draw();
    }

    pub fn update_status(&self) {
        let guard = self.state.borrow();
        let text = match guard.as_ref() {
            None => "Bibliothek".to_string(),
            Some(st) => {
                let name = st
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let pos = match st.pos {
                    ViewPos::Full(n) => format!("Seite {}/{}", n + 1, st.n_pages),
                    ViewPos::Split(n) => format!("Seite {}→{}/{}", n + 1, n + 2, st.n_pages),
                };
                let pen = if st.annotate { "  ✎" } else { "" };
                format!("{name} – {pos}{pen}")
            }
        };
        self.status.set_text(&text);
        if self.nav_box.is_visible() {
            self.sync_nav();
        }
    }

    fn setup_pen_button(&self) {
        let v = self.clone();
        self.pen_button.connect_toggled(move |btn| {
            let active = btn.is_active();
            {
                let mut guard = v.state.borrow_mut();
                let Some(st) = guard.as_mut() else { return };
                st.annotate = active;
                // Show the full current page for drawing; the overlay
                // would sit in the way of bottom-of-page strokes.
                if active {
                    st.pos = ViewPos::Full(st.pos.base_page());
                    v.nav_box.set_visible(false);
                }
            }
            if !active {
                // Leaving annotation mode saves.
                v.flush();
            }
            v.update_status();
            v.area.queue_draw();
        });
    }

    // ----- Drawing -----

    fn setup_draw(&self) {
        let v = self.clone();
        self.area.set_draw_func(move |da, cr, w, h| {
            let w = w as f64;
            let h = h as f64;
            let sf = da.scale_factor() as f64;
            cr.set_source_rgb(0.13, 0.13, 0.13);
            let _ = cr.paint();
            {
                let mut guard = v.state.borrow_mut();
                let Some(st) = guard.as_mut() else { return };
                match st.pos {
                    ViewPos::Full(n) => {
                        draw_full_page(cr, st, &v.cfg, n, w, h, sf);
                    }
                    ViewPos::Split(n) => {
                        // Top: upper half of the next page; bottom: lower half
                        // of the current page, separated by a line.
                        draw_half_page(cr, st, &v.cfg, n + 1, true, w, 0.0, h / 2.0, sf);
                        draw_half_page(cr, st, &v.cfg, n, false, w, h / 2.0, h, sf);
                        cr.set_source_rgb(0.4, 0.4, 0.4);
                        cr.set_line_width(2.0);
                        cr.move_to(0.0, h / 2.0);
                        cr.line_to(w, h / 2.0);
                        let _ = cr.stroke();
                    }
                }
            }
            // Pre-render neighboring pages while idle so the next page
            // turn is just a copy.
            v.schedule_prefetch();
            // Replace the scaled-up preview with a crisp render once the
            // pinch gesture is over.
            if v.pinch.borrow().is_none() {
                v.schedule_zoom_render();
            }
        });
    }

    /// Renders the zoomed viewport crisply in an idle callback (a no-op
    /// when not zoomed or the cached viewport is still valid).
    fn schedule_zoom_render(&self) {
        {
            let guard = self.state.borrow();
            let Some(st) = guard.as_ref() else { return };
            if self.zoom_job.get() || zoom_cache_valid(st, &self.area) {
                return;
            }
        }
        self.zoom_job.set(true);
        let v = self.clone();
        glib::idle_add_local_once(move || {
            v.zoom_job.set(false);
            let w = v.area.width() as f64;
            let h = v.area.height() as f64;
            let sf = v.area.scale_factor() as f64;
            let mut guard = v.state.borrow_mut();
            let Some(st) = guard.as_mut() else { return };
            if st.zoom <= 1.0 || w <= 0.0 || h <= 0.0 {
                return;
            }
            let ViewPos::Full(n) = st.pos else { return };
            render_zoom_view(st, n, w, h, sf);
            drop(guard);
            v.area.queue_draw();
        });
    }

    fn schedule_prefetch(&self) {
        let v = self.clone();
        glib::idle_add_local_once(move || {
            if v.prefetch_one() {
                v.schedule_prefetch();
            }
        });
    }

    /// Renders at most one missing page around the current position into
    /// the cache. Returns whether pages are still missing afterwards.
    fn prefetch_one(&self) -> bool {
        let w = self.area.width() as f64;
        let h = self.area.height() as f64;
        let sf = self.area.scale_factor() as f64;
        if w <= 0.0 || h <= 0.0 {
            return false;
        }
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return false };
        let base = st.pos.base_page();
        let lo = base.saturating_sub(1);
        let hi = (base + 2).min(st.n_pages - 1);
        // Release pages that are no longer needed.
        st.cache.retain(|k, _| (lo..=hi).contains(k));
        // Order: forward first (the likely direction), then backward.
        for n in (base..=hi).chain(lo..base) {
            if !cache_valid(st, n, w, h, sf) {
                ensure_cached(st, n, w, h, sf);
                let more = ((base..=hi).chain(lo..base)).any(|m| !cache_valid(st, m, w, h, sf));
                return more;
            }
        }
        false
    }

    // ----- Input (stylus, mouse, tap to turn) -----

    fn setup_gestures(&self) {
        // Stylus with pressure.
        let stylus = gtk::GestureStylus::new();
        let v = self.clone();
        stylus.connect_down(move |g, x, y| {
            let p = g.axis(gtk::gdk::AxisUse::Pressure).unwrap_or(0.5);
            v.stroke_begin(x, y, p);
        });
        let v = self.clone();
        stylus.connect_motion(move |g, x, y| {
            let p = g.axis(gtk::gdk::AxisUse::Pressure).unwrap_or(0.5);
            v.stroke_move(x, y, p);
        });
        let v = self.clone();
        stylus.connect_up(move |_, x, y| {
            v.stroke_end(x, y, 0.5);
        });
        self.area.add_controller(stylus);

        // Mouse fallback (for the couch/testing). Touch is denied (palm
        // rejection); the stylus is handled by the gesture above.
        let drag = gtk::GestureDrag::new();
        drag.set_button(gtk::gdk::BUTTON_PRIMARY);
        let v = self.clone();
        drag.connect_drag_begin(move |g, x, y| {
            if let Some(src) = event_source(g.upcast_ref::<gtk::EventController>()) {
                use gtk::gdk::InputSource;
                if matches!(
                    src,
                    InputSource::Touchscreen | InputSource::Pen | InputSource::TabletPad
                ) {
                    g.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
            }
            v.stroke_begin(x, y, 0.5);
        });
        let v = self.clone();
        drag.connect_drag_update(move |g, dx, dy| {
            if let Some((sx, sy)) = g.start_point() {
                v.stroke_move(sx + dx, sy + dy, 0.5);
            }
        });
        let v = self.clone();
        drag.connect_drag_end(move |g, dx, dy| {
            if let Some((sx, sy)) = g.start_point() {
                v.stroke_end(sx + dx, sy + dy, 0.5);
            }
        });
        self.area.add_controller(drag);

        // Two-finger pinch: zoom in before annotating, pinch out to get
        // back to the fitted view. Moving both fingers pans. Zoom resets
        // on page turns.
        let zoom_g = gtk::GestureZoom::new();
        let v = self.clone();
        zoom_g.connect_begin(move |g, _| {
            let w = v.area.width() as f64;
            let h = v.area.height() as f64;
            let guard = v.state.borrow();
            let Some(st) = guard.as_ref() else { return };
            let ViewPos::Full(n) = st.pos else { return };
            let Some(page) = st.doc.page(n as i32) else { return };
            let (pw, ph) = page.size();
            let (_, ox, oy) = view_transform(st, w, h, pw, ph);
            let (cx, cy) = g.bounding_box_center().unwrap_or((w / 2.0, h / 2.0));
            *v.pinch.borrow_mut() = Some(PinchStart {
                zoom: st.zoom,
                ox,
                oy,
                cx,
                cy,
            });
        });
        let v = self.clone();
        zoom_g.connect_scale_changed(move |g, factor| {
            let w = v.area.width() as f64;
            let h = v.area.height() as f64;
            let pinch = v.pinch.borrow();
            let Some(start) = pinch.as_ref() else { return };
            let mut guard = v.state.borrow_mut();
            let Some(st) = guard.as_mut() else { return };
            let ViewPos::Full(n) = st.pos else { return };
            let Some(page) = st.doc.page(n as i32) else { return };
            let (pw, ph) = page.size();
            let (s0, _, _) = full_layout(w, h, pw, ph);
            let new_zoom = (start.zoom * factor).clamp(1.0, MAX_ZOOM);
            if new_zoom <= 1.01 {
                reset_zoom(st);
            } else {
                // Keep the page point that was under the gesture center
                // fixed; following the current center also pans.
                let s_start = s0 * start.zoom;
                let px = (start.cx - start.ox) / s_start;
                let py = (start.cy - start.oy) / s_start;
                let (cx, cy) = g
                    .bounding_box_center()
                    .unwrap_or((start.cx, start.cy));
                let s_new = s0 * new_zoom;
                st.zoom = new_zoom;
                st.view = (cx - px * s_new, cy - py * s_new);
            }
            drop(guard);
            drop(pinch);
            v.area.queue_draw();
        });
        let v = self.clone();
        zoom_g.connect_end(move |_, _| {
            *v.pinch.borrow_mut() = None;
            v.area.queue_draw();
        });
        let v = self.clone();
        zoom_g.connect_cancel(move |_, _| {
            *v.pinch.borrow_mut() = None;
            v.area.queue_draw();
        });
        self.area.add_controller(zoom_g);

        // Tapping the edges turns pages (right third forward, left third
        // backward); the middle third toggles the overlay. Edge taps are
        // disabled while annotating or zoomed (strokes and panning must
        // not turn pages), but the middle tap keeps working — otherwise
        // pen mode in fullscreen would be inescapable without a
        // keyboard. While annotating, only finger taps open the overlay;
        // pen and mouse are drawing tools there.
        let click = gtk::GestureClick::new();
        let v = self.clone();
        click.connect_released(move |g, _, x, _| {
            let Some((annotating, zoomed)) = v
                .state
                .borrow()
                .as_ref()
                .map(|st| (st.annotate, st.zoom > 1.0))
            else {
                return;
            };
            let w = v.area.width() as f64;
            if (w / 3.0..=w * 2.0 / 3.0).contains(&x) {
                let src = event_source(g.upcast_ref::<gtk::EventController>());
                if !annotating || src == Some(gtk::gdk::InputSource::Touchscreen) {
                    v.toggle_nav();
                }
                return;
            }
            if annotating || zoomed {
                return;
            }
            if x > w * 2.0 / 3.0 {
                v.forward();
            } else {
                v.backward();
            }
        });
        self.area.add_controller(click);
    }

    fn stroke_begin(&self, x: f64, y: f64, pressure: f64) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        if !st.annotate {
            return;
        }
        if let Some(pt) = self.widget_to_page(st, x, y, pressure) {
            st.current = Some(Stroke { points: vec![pt] });
        }
        drop(guard);
        self.area.queue_draw();
    }

    fn stroke_move(&self, x: f64, y: f64, pressure: f64) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        if !st.annotate || st.current.is_none() {
            return;
        }
        if let (Some(pt), Some(cur)) = (
            self.widget_to_page(st, x, y, pressure),
            st.current.as_mut(),
        ) {
            cur.points.push(pt);
        }
        drop(guard);
        self.area.queue_draw();
    }

    fn stroke_end(&self, x: f64, y: f64, pressure: f64) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        if let Some(mut cur) = st.current.take() {
            if let Some(pt) = self.widget_to_page(st, x, y, pressure) {
                cur.points.push(pt);
            }
            st.pending.entry(st.pos.base_page()).or_default().push(cur);
        }
        drop(guard);
        self.area.queue_draw();
    }

    /// Widget coordinates → page coordinates (only in Full mode, which
    /// annotation mode enforces).
    fn widget_to_page(&self, st: &DocState, x: f64, y: f64, pressure: f64) -> Option<StrokePoint> {
        let ViewPos::Full(n) = st.pos else { return None };
        let page = st.doc.page(n as i32)?;
        let (pw, ph) = page.size();
        let (scale, ox, oy) = view_transform(
            st,
            self.area.width() as f64,
            self.area.height() as f64,
            pw,
            ph,
        );
        Some(StrokePoint {
            x: ((x - ox) / scale).clamp(0.0, pw),
            y: ((y - oy) / scale).clamp(0.0, ph),
            pressure,
        })
    }
}

fn load_document(path: &Path) -> Result<poppler::Document, String> {
    let uri = glib::filename_to_uri(path, None).map_err(|e| e.to_string())?;
    poppler::Document::from_file(uri.as_str(), None).map_err(|e| e.to_string())
}

/// Scale and offset to fit a page (pw×ph) centered into the area (w×h).
fn full_layout(w: f64, h: f64, pw: f64, ph: f64) -> (f64, f64, f64) {
    let scale = (w / pw).min(h / ph);
    ((scale), (w - pw * scale) / 2.0, (h - ph * scale) / 2.0)
}

/// Current transform of the full page view, including pinch zoom and
/// pan. Clamped so the page never leaves a gap at the viewport edges.
fn view_transform(st: &DocState, w: f64, h: f64, pw: f64, ph: f64) -> (f64, f64, f64) {
    let (s0, ox0, oy0) = full_layout(w, h, pw, ph);
    if st.zoom <= 1.0 {
        return (s0, ox0, oy0);
    }
    let s = s0 * st.zoom;
    (
        s,
        clamp_offset(st.view.0, w, pw * s),
        clamp_offset(st.view.1, h, ph * s),
    )
}

/// Clamps a pan offset: center the page if it is smaller than the
/// viewport in this dimension, otherwise keep the viewport covered.
fn clamp_offset(v: f64, viewport: f64, page_extent: f64) -> f64 {
    if page_extent <= viewport {
        (viewport - page_extent) / 2.0
    } else {
        v.clamp(viewport - page_extent, 0.0)
    }
}

fn reset_zoom(st: &mut DocState) {
    st.zoom = 1.0;
    st.view = (0.0, 0.0);
    st.zoom_cache = None;
}

/// Is the cached crisp viewport still valid for the current transform?
fn zoom_cache_valid(st: &DocState, area: &gtk::DrawingArea) -> bool {
    if st.zoom <= 1.0 {
        return true; // nothing to render
    }
    let w = area.width() as f64;
    let h = area.height() as f64;
    let sf = area.scale_factor() as f64;
    let ViewPos::Full(n) = st.pos else { return true };
    let Some(page) = st.doc.page(n as i32) else { return true };
    let (pw, ph) = page.size();
    let (_, ox, oy) = view_transform(st, w, h, pw, ph);
    st.zoom_cache
        .as_ref()
        .map(|z| {
            (z.zoom - st.zoom).abs() < 1e-9
                && (z.ox - ox).abs() < 0.01
                && (z.oy - oy).abs() < 0.01
                && z.w_px == (w * sf).ceil() as i32
                && z.h_px == (h * sf).ceil() as i32
                && (z.sf - sf).abs() < 1e-9
        })
        .unwrap_or(false)
}

/// Renders the currently visible zoomed viewport at full resolution.
fn render_zoom_view(st: &mut DocState, n: usize, w: f64, h: f64, sf: f64) {
    let Some(page) = st.doc.page(n as i32) else { return };
    let (pw, ph) = page.size();
    let (scale, ox, oy) = view_transform(st, w, h, pw, ph);
    let w_px = (w * sf).ceil() as i32;
    let h_px = (h * sf).ceil() as i32;
    if w_px <= 0 || h_px <= 0 {
        return;
    }
    let Ok(surface) = cairo::ImageSurface::create(cairo::Format::ARgb32, w_px, h_px) else {
        return;
    };
    {
        let Ok(cr) = cairo::Context::new(&surface) else { return };
        cr.scale(sf, sf);
        cr.set_source_rgb(0.13, 0.13, 0.13);
        let _ = cr.paint();
        cr.translate(ox, oy);
        cr.scale(scale, scale);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.rectangle(0.0, 0.0, pw, ph);
        let _ = cr.fill();
        page.render(&cr);
    }
    st.zoom_cache = Some(ZoomSurface {
        zoom: st.zoom,
        ox,
        oy,
        w_px,
        h_px,
        sf,
        surface,
    });
}

/// Pixel scale for the cache. In split mode the fit scale is identical to
/// the full view (half the height, half the page), so one cached bitmap
/// per page serves both modes.
fn cache_scale(st: &DocState, n: usize, w: f64, h: f64, sf: f64) -> Option<f64> {
    let page = st.doc.page(n as i32)?;
    let (pw, ph) = page.size();
    let (scale, _, _) = full_layout(w, h, pw, ph);
    Some(scale * sf)
}

fn cache_valid(st: &DocState, n: usize, w: f64, h: f64, sf: f64) -> bool {
    let Some(s_px) = cache_scale(st, n, w, h, sf) else {
        return true; // page not loadable – nothing to render
    };
    st.cache
        .get(&n)
        .map(|c| (c.s_px - s_px).abs() < 1e-6)
        .unwrap_or(false)
}

/// Thumbnail worker thread: opens its own Poppler document (Poppler is
/// not thread-safe, but exclusive use within one thread is fine) and
/// renders queued pages until the queue is empty or quit is set.
fn thumb_worker(path: PathBuf, shared: Arc<Mutex<ThumbState>>) {
    // The last worker to leave marks the whole job as done.
    let leave = |shared: &Arc<Mutex<ThumbState>>| {
        let mut g = shared.lock().unwrap();
        g.active -= 1;
        if g.active == 0 {
            g.done = true;
        }
    };
    let doc = glib::filename_to_uri(&path, None)
        .ok()
        .and_then(|uri| poppler::Document::from_file(uri.as_str(), None).ok());
    let Some(doc) = doc else {
        leave(&shared);
        return;
    };
    loop {
        let page = {
            let mut g = shared.lock().unwrap();
            if g.quit {
                drop(g);
                leave(&shared);
                return;
            }
            loop {
                match g.queue.pop_front() {
                    Some(p) if g.taken.contains(&p) => continue,
                    Some(p) => {
                        g.taken.insert(p);
                        break p;
                    }
                    None => {
                        drop(g);
                        leave(&shared);
                        return;
                    }
                }
            }
        };
        if let Some(pixels) = render_thumb(&doc, page) {
            let mut g = shared.lock().unwrap();
            if g.quit {
                drop(g);
                leave(&shared);
                return;
            }
            g.results.push(pixels);
        }
    }
}

/// Renders one page at thumbnail size into raw ARGB pixels.
fn render_thumb(doc: &poppler::Document, n: usize) -> Option<ThumbPixels> {
    let page = doc.page(n as i32)?;
    let (pw, ph) = page.size();
    let s = THUMB_H / ph;
    let w_px = (pw * s).ceil() as i32;
    let h_px = THUMB_H.ceil() as i32;
    if w_px <= 0 || h_px <= 0 {
        return None;
    }
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w_px, h_px).ok()?;
    {
        let cr = cairo::Context::new(&surface).ok()?;
        cr.scale(s, s);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let _ = cr.paint();
        page.render(&cr);
    }
    surface.flush();
    let stride = surface.stride();
    let data = surface.data().ok()?.to_vec();
    Some((n, w_px, h_px, stride, data))
}

/// Renders page n into the cache if it is missing or its scale is stale.
fn ensure_cached(st: &mut DocState, n: usize, w: f64, h: f64, sf: f64) {
    if cache_valid(st, n, w, h, sf) {
        return;
    }
    let Some(page) = st.doc.page(n as i32) else { return };
    let (pw, ph) = page.size();
    let Some(s_px) = cache_scale(st, n, w, h, sf) else { return };
    let w_px = (pw * s_px).ceil() as i32;
    let h_px = (ph * s_px).ceil() as i32;
    if w_px <= 0 || h_px <= 0 {
        return;
    }
    let Ok(surface) = cairo::ImageSurface::create(cairo::Format::ARgb32, w_px, h_px) else {
        return;
    };
    {
        let Ok(cr) = cairo::Context::new(&surface) else { return };
        cr.scale(s_px, s_px);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let _ = cr.paint();
        page.render(&cr);
    }
    st.cache.insert(n, CachedPage { s_px, surface });
}

/// Copies the cached bitmap of a page to (ox, oy) in widget coordinates
/// (1 surface pixel = 1 device pixel).
fn blit_page(cr: &cairo::Context, cached: &CachedPage, ox: f64, oy: f64, sf: f64) {
    cr.save().ok();
    cr.translate(ox, oy);
    cr.scale(1.0 / sf, 1.0 / sf);
    let _ = cr.set_source_surface(&cached.surface, 0.0, 0.0);
    let _ = cr.paint();
    cr.restore().ok();
}

fn draw_full_page(
    cr: &cairo::Context,
    st: &mut DocState,
    cfg: &Config,
    n: usize,
    w: f64,
    h: f64,
    sf: f64,
) {
    let Some(page) = st.doc.page(n as i32) else { return };
    let (pw, ph) = page.size();
    let (scale, ox, oy) = view_transform(st, w, h, pw, ph);
    if st.zoom <= 1.0 {
        ensure_cached(st, n, w, h, sf);
        if let Some(cached) = st.cache.get(&n) {
            blit_page(cr, cached, ox, oy, sf);
        }
    } else if let Some(z) = st.zoom_cache.as_ref().filter(|z| {
        (z.zoom - st.zoom).abs() < 1e-9
            && (z.ox - ox).abs() < 0.01
            && (z.oy - oy).abs() < 0.01
            && (z.sf - sf).abs() < 1e-9
            && z.w_px == (w * sf).ceil() as i32
            && z.h_px == (h * sf).ceil() as i32
    }) {
        // Crisp viewport render, aligned with the widget origin.
        cr.save().ok();
        cr.scale(1.0 / sf, 1.0 / sf);
        let _ = cr.set_source_surface(&z.surface, 0.0, 0.0);
        let _ = cr.paint();
        cr.restore().ok();
    } else {
        // Preview while pinching (or until the idle job finishes): scale
        // up the fitted bitmap. Blurry, but instant.
        ensure_cached(st, n, w, h, sf);
        if let Some(cached) = st.cache.get(&n) {
            blit_page(cr, cached, ox, oy, sf / st.zoom);
        }
    }
    cr.save().ok();
    cr.translate(ox, oy);
    cr.scale(scale, scale);
    draw_strokes(cr, st, cfg, n, true);
    cr.restore().ok();
}

/// Draws the top (`top == true`) or bottom half of page `n` into the
/// region y0..y1 of the drawing area.
#[allow(clippy::too_many_arguments)]
fn draw_half_page(
    cr: &cairo::Context,
    st: &mut DocState,
    cfg: &Config,
    n: usize,
    top: bool,
    w: f64,
    y0: f64,
    y1: f64,
    sf: f64,
) {
    let Some(page) = st.doc.page(n as i32) else { return };
    let (pw, ph) = page.size();
    let region_h = y1 - y0;
    let scale = (w / pw).min(region_h / (ph / 2.0));
    let ox = (w - pw * scale) / 2.0;
    // Top half: align the page start with the top; bottom half: align the
    // page end with the bottom so nothing gets cut off.
    let oy = if top { y0 } else { y1 - ph * scale };
    // Half a page in half the area = the same scale as a full page in the
    // full area (2·region_h) – so the full-view bitmap fits.
    ensure_cached(st, n, w, 2.0 * region_h, sf);
    cr.save().ok();
    cr.rectangle(0.0, y0, w, region_h);
    cr.clip();
    if let Some(cached) = st.cache.get(&n) {
        blit_page(cr, cached, ox, oy, sf);
    }
    cr.translate(ox, oy);
    cr.scale(scale, scale);
    draw_strokes(cr, st, cfg, n, false);
    cr.restore().ok();
}

/// Draws the not-yet-burned strokes of a page; the cairo coordinate
/// system must already be the page's.
fn draw_strokes(
    cr: &cairo::Context,
    st: &DocState,
    cfg: &Config,
    page_idx: usize,
    include_current: bool,
) {
    let (r, g, b) = cfg.pen_rgb();
    cr.set_source_rgb(r, g, b);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);

    let pending = st.pending.get(&page_idx).map(|v| v.as_slice()).unwrap_or(&[]);
    let current = if include_current && st.pos.base_page() == page_idx {
        st.current.as_ref()
    } else {
        None
    };
    for stroke in pending.iter().chain(current) {
        match stroke.points.len() {
            0 => {}
            1 => {
                let p = &stroke.points[0];
                let w = burn::width_for(cfg.pen_width, p.pressure);
                cr.set_line_width(w);
                cr.move_to(p.x, p.y);
                cr.line_to(p.x, p.y);
                let _ = cr.stroke();
            }
            _ => {
                for pair in stroke.points.windows(2) {
                    let w = burn::width_for(
                        cfg.pen_width,
                        (pair[0].pressure + pair[1].pressure) / 2.0,
                    );
                    cr.set_line_width(w);
                    cr.move_to(pair[0].x, pair[0].y);
                    cr.line_to(pair[1].x, pair[1].y);
                    let _ = cr.stroke();
                }
            }
        }
    }
}

/// Source (mouse/touch/pen) of a controller's current event.
fn event_source(controller: &gtk::EventController) -> Option<gtk::gdk::InputSource> {
    controller
        .current_event()
        .and_then(|ev| ev.device())
        .map(|dev| dev.source())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Object, Stream};

    /// Renders the given view position into a PNG, exactly like the
    /// draw func does, so split rendering can be checked headlessly.
    fn render_view(st: &mut DocState, cfg: &Config, w: f64, h: f64, out: &Path) {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32).unwrap();
        {
            let cr = cairo::Context::new(&surface).unwrap();
            cr.set_source_rgb(0.13, 0.13, 0.13);
            cr.paint().unwrap();
            match st.pos {
                ViewPos::Full(n) => draw_full_page(&cr, st, cfg, n, w, h, 1.0),
                ViewPos::Split(n) => {
                    draw_half_page(&cr, st, cfg, n + 1, true, w, 0.0, h / 2.0, 1.0);
                    draw_half_page(&cr, st, cfg, n, false, w, h / 2.0, h, 1.0);
                }
            }
        }
        let mut f = std::fs::File::create(out).unwrap();
        surface.write_to_png(&mut f).unwrap();
    }

    /// Two pages: page 1 has a black bar at the top, page 2 a black bar
    /// at the bottom (in display coordinates).
    fn make_two_page_pdf(path: &Path) {
        let mut doc = lopdf::Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        // Page 1: bar near the page top (PDF y ~ 700).
        let c1 = doc.add_object(Stream::new(
            dictionary! {},
            b"0 0 0 rg 50 680 512 60 re f".to_vec(),
        ));
        // Page 2: bar near the page bottom (PDF y ~ 60).
        let c2 = doc.add_object(Stream::new(
            dictionary! {},
            b"0 0 0 rg 50 50 512 60 re f".to_vec(),
        ));
        let p1 = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => c1,
        });
        let p2 = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => c2,
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![p1.into(), p2.into()],
            "Count" => 2,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(path).unwrap();
    }

    #[test]
    fn split_view_renders_both_halves() {
        let dir = std::env::temp_dir().join(format!("frack-view-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("two.pdf");
        make_two_page_pdf(&pdf);
        let doc = load_document(&pdf).unwrap();
        let mut st = DocState {
            path: pdf.clone(),
            doc,
            n_pages: 2,
            pos: ViewPos::Split(0),
            annotate: false,
            pending: BTreeMap::new(),
            current: None,
            cache: BTreeMap::new(),
            thumbs: BTreeMap::new(),
            zoom: 1.0,
            view: (0.0, 0.0),
            zoom_cache: None,
        };
        let cfg = Config::default();
        let (w, h) = (300.0, 400.0);
        let out = if let Ok(d) = std::env::var("FRACK_TEST_OUT") {
            PathBuf::from(d).join("split.png")
        } else {
            dir.join("split.png")
        };
        render_view(&mut st, &cfg, w, h, &out);

        // Check pixels: split shows the top half of page 2 (its bar is in
        // the bottom half, so the top region stays white) above the
        // bottom half of page 1 (its bar is at the page top, so the
        // bottom region stays white as well). White in both sampled rows
        // means the correct halves are shown; black would mean the wrong
        // half is displayed.
        let mut surface = {
            let mut f = std::fs::File::open(&out).unwrap();
            cairo::ImageSurface::create_from_png(&mut f).unwrap()
        };
        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        let px = |x: usize, y: usize| -> (u8, u8, u8) {
            let o = y * stride + x * 4;
            (data[o + 2], data[o + 1], data[o])
        };
        // Top quarter (page 2, upper half): must be white.
        let (r, g, b) = px(150, 40);
        assert!(r > 200 && g > 200 && b > 200, "top region not white: {r},{g},{b}");
        // Bottom quarter (page 1, lower half): must be white.
        let (r, g, b) = px(150, 360);
        assert!(r > 200 && g > 200 && b > 200, "bottom region not white: {r},{g},{b}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
