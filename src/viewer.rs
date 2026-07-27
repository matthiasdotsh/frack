// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

//! Displays a PDF with half-page turns (the next page's top half appears
//! first) and freehand annotations.
//!
//! Annotations are standard PDF ink annotations (see annot.rs). While a
//! document is open, all of its strokes live in memory as the single
//! source of truth: on open they are read out of the file and removed from
//! the copy Poppler renders, so frack draws them itself as an overlay it
//! fully controls. This makes undo unlimited (and effective across
//! sessions, since the strokes are in the file) and redo possible within a
//! session. Strokes are written back on page turns, on close, and by a
//! debounced autosave a few seconds after the pen is lifted – so even a
//! single-page document, or an unexpected shutdown, keeps its annotations.
//!
//! Input: the stylus always draws (no mode needed); the pen button turns
//! finger drawing on. Two fingers zoom in either mode. See [`Viewer::setup_gestures`].
//!
//! Rendered pages are cached as bitmaps and neighboring pages are
//! pre-rendered while idle – turning a page only copies pixels.

use crate::annot::{self, PageChange, Stroke, StrokePoint, StrokesByPage};
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
use std::time::Duration;

/// Delay between the last stroke and an automatic save. Debounced: every
/// new stroke pushes it back, so the save lands in a pause, never mid-write.
const AUTOSAVE_DELAY: Duration = Duration::from_secs(10);

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

/// Eraser radius in page points: a stroke is erased when the eraser comes
/// this close to it (plus the stroke's own half width).
const ERASER_RADIUS: f64 = 10.0;

/// One reversible edit on a page's stroke list, for undo/redo. Positions
/// are indices into the page's `strokes` vector at the time of the edit;
/// because undo/redo happen strictly in reverse order, the list always
/// matches the state an edit was recorded in, so the indices stay valid.
enum Edit {
    /// A stroke was added at this index (drawing, or a redone erase-undo).
    Added(usize, Stroke),
    /// One or more strokes were partly erased: each was replaced in place by
    /// its remaining pieces. Recorded in application order; undo reverses
    /// them in reverse order (so indices stay valid).
    Trimmed(Vec<TrimOp>),
}

/// One stroke replaced in place by the pieces the eraser left of it (empty
/// if it was erased entirely).
struct TrimOp {
    index: usize,
    original: Stroke,
    pieces: Vec<Stroke>,
}

/// The erase gesture in progress: which page, the trims done so far (to
/// become one undo step), and the eraser's widget position (for the cursor
/// circle).
struct ErasePass {
    page: usize,
    ops: Vec<TrimOp>,
    cursor: (f64, f64),
}

/// What a pointer contact does.
enum Action {
    Draw,
    Erase,
    Ignore,
}

/// Render height of slider preview thumbnails (pixels).
const THUMB_H: f64 = 240.0;
/// Content size of the preview tile above the slider.
const PREVIEW_W: i32 = 190;
const PREVIEW_H: i32 = 250;

pub struct DocState {
    pub path: PathBuf,
    pub doc: poppler::Document,
    pub n_pages: usize,
    /// First/last 0-based page the view is bounded to – a setlist page range,
    /// or the whole document. Turning and the slider stay within `lo..=hi`.
    lo: usize,
    hi: usize,
    pub pos: ViewPos,
    /// Whether finger drawing is on (the pen button). The stylus draws
    /// regardless; this only affects touch and mouse input.
    pub annotate: bool,
    /// Whether finger erasing is on (the eraser button). The stylus' eraser
    /// end erases regardless; this only affects touch and mouse input.
    pub erase: bool,
    /// All strokes of the document, per 0-based page index – the source of
    /// truth while it is open. Drawn as an overlay; the same strokes are
    /// removed from the Poppler render on load, so nothing appears twice.
    pub strokes: BTreeMap<usize, Vec<Stroke>>,
    /// How many of a page's strokes are already written to the file (the
    /// front of the list). Anything beyond is appended on the next save.
    saved: BTreeMap<usize, usize>,
    /// Pages where a stroke was removed and which therefore need to be
    /// rewritten from scratch (rather than appended to) on the next save.
    rewrite: HashSet<usize>,
    /// Per-page undo stack of edits, newest last. Seeded on open with an
    /// `Added` per existing stroke, so undo reaches strokes saved earlier
    /// (even in a previous session).
    undo: BTreeMap<usize, Vec<Edit>>,
    /// Per-page redo stack. In memory only – redo does not survive closing.
    redo: BTreeMap<usize, Vec<Edit>>,
    /// The stroke currently being drawn.
    pub current: Option<Stroke>,
    /// The erase gesture in progress, if any.
    erasing: Option<ErasePass>,
    /// True while a stylus stroke (draw or erase) is in progress, so
    /// resting-palm touches are ignored (palm rejection).
    stylus_active: bool,
    cache: BTreeMap<usize, CachedPage>,
    /// Small page renders for the slider preview, cached per page.
    thumbs: BTreeMap<usize, cairo::ImageSurface>,
    /// Pinch zoom factor; 1.0 = page fits the area (no zoom).
    zoom: f64,
    /// Widget-space translation of the page origin while zoomed.
    view: (f64, f64),
    zoom_cache: Option<ZoomSurface>,
}

/// A 1-based, inclusive page range for a playlist entry; `None` on a side is an
/// open end. Resolved against the document's page count when it opens.
#[derive(Clone, Copy)]
pub struct PageRange {
    pub lo: Option<usize>,
    pub hi: Option<usize>,
}

/// One entry of a playlist (setlist) the viewer can turn across.
#[derive(Clone)]
pub struct PlaylistEntry {
    pub path: PathBuf,
    /// Restrict the view to these pages; `None` shows the whole file.
    pub range: Option<PageRange>,
    /// Known-broken before opening (missing file or an invalid range): shows a
    /// placeholder page instead of loading.
    pub broken: bool,
    /// Human label (relative path) for the status line and placeholder page.
    pub label: String,
}

/// Callback invoked with the new index when turning crosses into another piece.
type PieceChangeCb = Rc<RefCell<Option<Box<dyn Fn(usize)>>>>;

/// A setlist opened for turning across its pieces.
pub struct Playlist {
    pub entries: Vec<PlaylistEntry>,
    pub index: usize,
}

#[derive(Clone)]
pub struct Viewer {
    pub area: gtk::DrawingArea,
    pub state: Rc<RefCell<Option<DocState>>>,
    /// The setlist being played, if any: turning past a piece's edge moves to
    /// the neighbouring entry. `None` for a directly opened file.
    playlist: Rc<RefCell<Option<Playlist>>>,
    /// Called with the new index whenever turning crosses into another piece.
    on_piece_change: PieceChangeCb,
    /// Label of a missing/broken setlist entry shown as a placeholder page;
    /// `None` when a real document (or nothing) is shown.
    placeholder: Rc<RefCell<Option<String>>>,
    cfg: Rc<Config>,
    status: gtk::Label,
    pen_button: gtk::ToggleButton,
    erase_button: gtk::ToggleButton,
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
    /// Overlay line mirroring the header status, which is hidden in fullscreen:
    /// the current piece's name, prefixed with the setlist position
    /// ("2/3 · name") while a playlist is playing. Shown whenever a document is
    /// open, so on stage you always see what you are on.
    overlay_title: gtk::Label,
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
    /// Undo/redo buttons (header + overlay mirrors) whose sensitivity
    /// tracks the current page's history. Registered by main.
    undo_buttons: Rc<RefCell<Vec<gtk::Button>>>,
    redo_buttons: Rc<RefCell<Vec<gtk::Button>>>,
    /// Pending debounced autosave, if armed.
    autosave_timer: Rc<RefCell<Option<glib::SourceId>>>,
    /// Whether the stylus tool in proximity is the eraser end. GTK4/Wayland
    /// can apply a pen↔eraser tool change a beat late, so the button-down
    /// event may still report the old tool; the proximity signal (which
    /// fires as the tool nears the screen) updates this first. See
    /// `setup_gestures`.
    stylus_eraser: Rc<std::cell::Cell<bool>>,
}

impl Viewer {
    pub fn new(
        cfg: Rc<Config>,
        status: gtk::Label,
        pen_button: gtk::ToggleButton,
        erase_button: gtk::ToggleButton,
    ) -> Self {
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
        // Title line at the top of the overlay (see field docs).
        let overlay_title = gtk::Label::new(None);
        overlay_title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        overlay_title.set_halign(gtk::Align::Center);
        overlay_title.set_visible(false);
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
        nav_box.append(&overlay_title);
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
            playlist: Rc::new(RefCell::new(None)),
            on_piece_change: Rc::new(RefCell::new(None)),
            placeholder: Rc::new(RefCell::new(None)),
            cfg,
            status,
            pen_button,
            erase_button,
            pinch: Rc::new(RefCell::new(None)),
            zoom_job: Rc::new(std::cell::Cell::new(false)),
            overlay,
            nav_box,
            nav_scale,
            nav_label,
            nav_actions,
            overlay_title,
            nav_updating: Rc::new(std::cell::Cell::new(false)),
            nav_timer: Rc::new(RefCell::new(None)),
            preview_box,
            preview_area,
            preview_label,
            preview_page: Rc::new(std::cell::Cell::new(0)),
            thumb_worker: Rc::new(RefCell::new(None)),
            undo_buttons: Rc::new(RefCell::new(Vec::new())),
            redo_buttons: Rc::new(RefCell::new(Vec::new())),
            autosave_timer: Rc::new(RefCell::new(None)),
            stylus_eraser: Rc::new(std::cell::Cell::new(false)),
        };
        viewer.setup_draw();
        viewer.setup_gestures();
        viewer.setup_pen_button();
        viewer.setup_erase_button();
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

    /// Opens a single file directly, discarding any playlist.
    pub fn open(&self, path: &Path) -> Result<(), String> {
        self.playlist.borrow_mut().take();
        self.load(path, None)
    }

    /// Opens a setlist starting at `index`; turning past a piece's edge then
    /// crosses to the neighbouring entry.
    pub fn open_playlist(&self, entries: Vec<PlaylistEntry>, index: usize) -> Result<(), String> {
        let entry = entries
            .get(index)
            .cloned()
            .ok_or_else(|| "empty playlist".to_string())?;
        *self.playlist.borrow_mut() = Some(Playlist { entries, index });
        self.open_entry(&entry);
        self.notify_piece_change(index);
        Ok(())
    }

    /// Opens a playlist entry: a real document, or a placeholder page when it is
    /// missing, unreadable, or its range lies past the end of the file.
    fn open_entry(&self, entry: &PlaylistEntry) {
        if entry.broken || self.load(&entry.path, entry.range).is_err() {
            self.set_placeholder(&entry.label);
        }
    }

    /// Shows a placeholder page for a missing/broken entry (never crashes or
    /// silently skips – you can turn right past it).
    fn set_placeholder(&self, label: &str) {
        self.close(); // flush & drop the previous piece
        *self.placeholder.borrow_mut() = Some(label.to_string());
        self.pen_button.set_active(false);
        self.erase_button.set_active(false);
        self.update_status();
        self.area.queue_draw();
    }

    /// Registers a callback invoked with the new index whenever turning
    /// crosses into another piece (and when a playlist is first opened).
    pub fn set_on_piece_change(&self, f: impl Fn(usize) + 'static) {
        *self.on_piece_change.borrow_mut() = Some(Box::new(f));
    }

    fn notify_piece_change(&self, index: usize) {
        if let Some(f) = self.on_piece_change.borrow().as_ref() {
            f(index);
        }
    }

    /// Loads a document into the viewer (shared by direct open and playlist
    /// turning); does not touch the playlist. `range` bounds the view to a
    /// setlist page range (`None` = whole file).
    fn load(&self, path: &Path, range: Option<PageRange>) -> Result<(), String> {
        self.close();
        let abs = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
        let (doc, strokes) = load_document(&abs)?;
        let n_pages = with_poppler(|| doc.n_pages()).max(0) as usize;
        if n_pages == 0 {
            return Err("PDF has no pages".to_string());
        }
        // A range whose first page is past the end is treated as broken.
        if range_past_end(range, n_pages) {
            return Err("page range past the end of the document".to_string());
        }
        let (lo, hi) = bounds_for(range, n_pages);
        // Everything read from the file is already persisted.
        let saved = strokes.iter().map(|(&p, v)| (p, v.len())).collect();
        // Seed the undo history so already-saved strokes are undoable too.
        let undo = strokes
            .iter()
            .map(|(&p, v)| {
                let edits = v
                    .iter()
                    .enumerate()
                    .map(|(i, s)| Edit::Added(i, s.clone()))
                    .collect();
                (p, edits)
            })
            .collect();
        *self.state.borrow_mut() = Some(DocState {
            path: abs,
            doc,
            n_pages,
            lo,
            hi,
            pos: ViewPos::Full(lo),
            annotate: false,
            erase: false,
            strokes,
            saved,
            rewrite: HashSet::new(),
            undo,
            redo: BTreeMap::new(),
            current: None,
            erasing: None,
            stylus_active: false,
            cache: BTreeMap::new(),
            thumbs: BTreeMap::new(),
            zoom: 1.0,
            view: (0.0, 0.0),
            zoom_cache: None,
        });
        self.pen_button.set_active(false);
        self.erase_button.set_active(false);
        self.nav_updating.set(true);
        // The slider spans the visible range only; max(lo+2) keeps min < max
        // even for a single-page range (goto_page still clamps to lo..=hi).
        self.nav_scale
            .set_range((lo + 1) as f64, (hi + 1).max(lo + 2) as f64);
        self.nav_scale.set_value((lo + 1) as f64);
        self.nav_updating.set(false);
        self.nav_box.set_visible(false);
        self.update_status();
        self.update_history();
        self.area.queue_draw();
        // Generate slider previews in the background right away, so they
        // are ready by the time the slider is first used.
        self.start_thumbs();
        Ok(())
    }

    /// Registers a button whose sensitivity should follow whether the
    /// current page has something to undo / redo.
    pub fn register_undo_button(&self, b: &gtk::Button) {
        self.undo_buttons.borrow_mut().push(b.clone());
    }

    pub fn register_redo_button(&self, b: &gtk::Button) {
        self.redo_buttons.borrow_mut().push(b.clone());
    }

    /// Enables/disables the undo and redo buttons for the current page.
    fn update_history(&self) {
        let (can_undo, can_redo) = match self.state.borrow().as_ref() {
            Some(st) => {
                let page = st.pos.base_page();
                let undo = st.current.is_some()
                    || st.undo.get(&page).is_some_and(|v| !v.is_empty());
                let redo = st.redo.get(&page).is_some_and(|v| !v.is_empty());
                (undo, redo)
            }
            None => (false, false),
        };
        for b in self.undo_buttons.borrow().iter() {
            b.set_sensitive(can_undo);
        }
        for b in self.redo_buttons.borrow().iter() {
            b.set_sensitive(can_redo);
        }
    }

    /// (Re)arms the debounced autosave; called after each finished stroke.
    fn arm_autosave(&self) {
        if let Some(id) = self.autosave_timer.borrow_mut().take() {
            id.remove();
        }
        let v = self.clone();
        let id = glib::timeout_add_local_once(AUTOSAVE_DELAY, move || {
            *v.autosave_timer.borrow_mut() = None;
            v.flush();
        });
        *self.autosave_timer.borrow_mut() = Some(id);
    }

    fn cancel_autosave(&self) {
        if let Some(id) = self.autosave_timer.borrow_mut().take() {
            id.remove();
        }
    }

    /// Jumps directly to a 0-based page (full-page view).
    pub fn goto_page(&self, page: usize) {
        self.flush();
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        let page = page.clamp(st.lo, st.hi);
        if st.pos == ViewPos::Full(page) {
            return;
        }
        st.pos = ViewPos::Full(page);
        reset_zoom(st);
        drop(guard);
        self.update_status();
        self.update_history();
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
            let (lo, hi, n) = v
                .state
                .borrow()
                .as_ref()
                .map(|st| (st.lo, st.hi, st.n_pages))
                .unwrap_or((0, 0, 1));
            let target = target.clamp(lo + 1, hi + 1);
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

    /// Saves annotations and closes the document.
    pub fn close(&self) {
        self.flush();
        self.stop_thumbs();
        *self.state.borrow_mut() = None;
        self.placeholder.borrow_mut().take();
        self.nav_box.set_visible(false);
        self.update_history();
        self.area.queue_draw();
    }

    /// Writes strokes to the file: newly drawn ones are appended, pages
    /// with a removed stroke are rewritten. The rendered document is *not*
    /// reloaded – the in-memory strokes stay the source of truth, so
    /// nothing flickers and there is no re-render cost.
    pub fn flush(&self) {
        self.cancel_autosave();
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        // Commit an in-progress stroke (e.g. when a shortcut fires mid-draw).
        if let Some(cur) = st.current.take() {
            let page = st.pos.base_page();
            st.strokes.entry(page).or_default().push(cur);
            st.redo.remove(&page);
        }

        let mut changes: BTreeMap<usize, PageChange> = BTreeMap::new();
        for (&page, v) in &st.strokes {
            let saved = st.saved.get(&page).copied().unwrap_or(0);
            if st.rewrite.contains(&page) {
                changes.insert(page, PageChange::Rewrite(v.clone()));
            } else if v.len() > saved {
                changes.insert(page, PageChange::Append(v[saved..].to_vec()));
            }
        }
        if changes.is_empty() {
            return;
        }
        match annot::save_changes(&st.path, &changes) {
            Ok(()) => {
                for page in changes.keys() {
                    let len = st.strokes.get(page).map(Vec::len).unwrap_or(0);
                    st.saved.insert(*page, len);
                }
                st.rewrite.clear();
            }
            Err(e) => {
                drop(guard);
                self.status
                    .set_text(&format!("Fehler beim Speichern: {e}"));
                return;
            }
        }
        drop(guard);
        self.update_history();
    }

    /// True when the view sits on the last page – nothing further to turn to
    /// within this document.
    fn at_end(&self) -> bool {
        self.state
            .borrow()
            .as_ref()
            .is_some_and(|st| pos_at_end(st.pos, st.hi))
    }

    /// True when the view sits on the first page of the range.
    fn at_start(&self) -> bool {
        self.state
            .borrow()
            .as_ref()
            .is_some_and(|st| pos_at_start(st.pos, st.lo))
    }

    /// Moves `dir` (+1 / -1) entries in the playlist, opening that piece at its
    /// first page (forward) or last page (backward). Returns false when there
    /// is no playlist or no neighbour that way.
    fn playlist_step(&self, dir: i32) -> bool {
        let (ni, entry) = {
            let pl = self.playlist.borrow();
            let Some(pl) = pl.as_ref() else { return false };
            let Some(ni) = step_index(pl.index, pl.entries.len(), dir) else {
                return false;
            };
            (ni, pl.entries[ni].clone())
        };
        if let Some(pl) = self.playlist.borrow_mut().as_mut() {
            pl.index = ni;
        }
        self.open_entry(&entry);
        // Turning back lands on the previous piece's last page.
        if dir < 0
            && let Some(st) = self.state.borrow_mut().as_mut()
        {
            st.pos = ViewPos::Full(st.hi);
        }
        self.update_status();
        self.notify_piece_change(ni);
        self.area.queue_draw();
        true
    }

    pub fn forward(&self) {
        // On a placeholder, or at the last page, a foot pedal turns on to the
        // next piece.
        if self.placeholder.borrow().is_some() {
            self.playlist_step(1);
            return;
        }
        if self.at_end() && self.playlist_step(1) {
            return;
        }
        self.flush();
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        let editing = st.annotate || st.erase;
        st.pos = match st.pos {
            // While editing, turn whole pages (drawing/erasing needs one).
            ViewPos::Full(n) if editing && n < st.hi => ViewPos::Full(n + 1),
            ViewPos::Full(n) if !editing && n < st.hi => ViewPos::Split(n),
            ViewPos::Split(n) => ViewPos::Full(n + 1),
            other => other,
        };
        reset_zoom(st);
        drop(guard);
        self.update_status();
        self.update_history();
        self.area.queue_draw();
    }

    pub fn backward(&self) {
        // On a placeholder, or at the first page, turn back into the previous
        // piece's last page.
        if self.placeholder.borrow().is_some() {
            self.playlist_step(-1);
            return;
        }
        if self.at_start() && self.playlist_step(-1) {
            return;
        }
        self.flush();
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        let editing = st.annotate || st.erase;
        st.pos = match st.pos {
            ViewPos::Full(n) if editing && n > st.lo => ViewPos::Full(n - 1),
            ViewPos::Full(n) if !editing && n > st.lo => ViewPos::Split(n - 1),
            ViewPos::Split(n) => ViewPos::Full(n),
            other => other,
        };
        reset_zoom(st);
        drop(guard);
        self.update_status();
        self.update_history();
        self.area.queue_draw();
    }

    /// Undoes the last edit on the current page: cancels a stroke in
    /// progress, otherwise reverses the last draw or erase. Works on strokes
    /// saved earlier too (even from a previous session) – any change here
    /// marks the page to be rewritten on the next save.
    pub fn undo(&self) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        // A stroke still being drawn is dropped without touching history.
        if st.current.take().is_some() {
            drop(guard);
            self.area.queue_draw();
            self.update_history();
            return;
        }
        let page = st.pos.base_page();
        let Some(edit) = st.undo.get_mut(&page).and_then(Vec::pop) else {
            drop(guard);
            return;
        };
        let strokes = st.strokes.entry(page).or_default();
        match &edit {
            Edit::Added(i, _) => {
                if *i < strokes.len() {
                    strokes.remove(*i);
                }
            }
            Edit::Trimmed(ops) => {
                // Reverse each trim, newest first: drop the pieces, put the
                // original back.
                for op in ops.iter().rev() {
                    let i = op.index.min(strokes.len());
                    let end = (i + op.pieces.len()).min(strokes.len());
                    strokes.drain(i..end);
                    strokes.insert(i, op.original.clone());
                }
            }
        }
        st.redo.entry(page).or_default().push(edit);
        st.rewrite.insert(page);
        drop(guard);
        self.arm_autosave();
        self.area.queue_draw();
        self.update_history();
    }

    /// Redoes the last undone edit on the current page (within this session).
    pub fn redo(&self) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        let page = st.pos.base_page();
        let Some(edit) = st.redo.get_mut(&page).and_then(Vec::pop) else {
            drop(guard);
            return;
        };
        let strokes = st.strokes.entry(page).or_default();
        match &edit {
            Edit::Added(i, s) => {
                let i = (*i).min(strokes.len());
                strokes.insert(i, s.clone());
            }
            Edit::Trimmed(ops) => {
                // Replay each trim in order: drop the original, splice in the
                // pieces.
                for op in ops {
                    let i = op.index.min(strokes.len());
                    if i < strokes.len() {
                        strokes.remove(i);
                    }
                    for (k, p) in op.pieces.iter().enumerate() {
                        strokes.insert(i + k, p.clone());
                    }
                }
            }
        }
        st.undo.entry(page).or_default().push(edit);
        st.rewrite.insert(page);
        drop(guard);
        self.arm_autosave();
        self.area.queue_draw();
        self.update_history();
    }

    pub fn update_status(&self) {
        let guard = self.state.borrow();
        let text = match guard.as_ref() {
            None => match self.placeholder.borrow().as_ref() {
                Some(label) => {
                    let prefix = self
                        .playlist
                        .borrow()
                        .as_ref()
                        .map(|pl| format!("{}/{} · ", pl.index + 1, pl.entries.len()))
                        .unwrap_or_default();
                    let t = format!("{prefix}fehlt: {label}");
                    self.overlay_title.set_text(&t);
                    self.overlay_title.set_visible(true);
                    t
                }
                None => {
                    self.overlay_title.set_visible(false);
                    "Bibliothek".to_string()
                }
            },
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
                let prefix = self
                    .playlist
                    .borrow()
                    .as_ref()
                    .map(|pl| format!("{}/{} · ", pl.index + 1, pl.entries.len()))
                    .unwrap_or_default();
                // The header status is hidden in fullscreen; mirror the piece
                // name (with the setlist position, if any) into the overlay.
                self.overlay_title.set_text(&format!("{prefix}{name}"));
                self.overlay_title.set_visible(true);
                format!("{prefix}{name} – {pos}{pen}")
            }
        };
        self.status.set_text(&text);
        if self.nav_box.is_visible() {
            self.sync_nav();
        }
    }

    /// The pen button enables *finger* drawing. The stylus draws whether it
    /// is on or off; the button only decides whether touch and mouse draw
    /// too. Enabling it switches to the full page (drawing needs it and the
    /// split view's overlay would cover bottom-of-page strokes).
    fn setup_pen_button(&self) {
        let v = self.clone();
        self.pen_button.connect_toggled(move |btn| {
            let active = btn.is_active();
            {
                let mut guard = v.state.borrow_mut();
                let Some(st) = guard.as_mut() else { return };
                st.annotate = active;
                if active {
                    st.pos = ViewPos::Full(st.pos.base_page());
                    v.nav_box.set_visible(false);
                }
            }
            // Draw and erase modes are mutually exclusive.
            if active {
                v.erase_button.set_active(false);
            } else {
                // Turning finger drawing off is a natural "done" signal –
                // save now instead of waiting for the autosave.
                v.flush();
            }
            v.update_status();
            v.update_history();
            v.area.queue_draw();
        });
    }

    /// The eraser button enables finger/mouse erasing. The stylus' eraser
    /// end erases regardless. Mutually exclusive with the pen button.
    fn setup_erase_button(&self) {
        let v = self.clone();
        self.erase_button.connect_toggled(move |btn| {
            let active = btn.is_active();
            {
                let mut guard = v.state.borrow_mut();
                let Some(st) = guard.as_mut() else { return };
                st.erase = active;
                if active {
                    st.pos = ViewPos::Full(st.pos.base_page());
                    v.nav_box.set_visible(false);
                }
            }
            if active {
                v.pen_button.set_active(false);
            } else {
                v.flush();
            }
            v.update_status();
            v.update_history();
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
            // Fill the whole area with paper white so the letterbox around
            // a fitted page matches the page itself instead of showing dark
            // bars (most visible for a portrait page on a portrait screen).
            cr.set_source_rgb(1.0, 1.0, 1.0);
            let _ = cr.paint();
            // A missing/broken setlist entry: draw its placeholder and stop.
            if v.state.borrow().is_none() {
                if let Some(label) = v.placeholder.borrow().as_ref() {
                    draw_placeholder(cr, w, h, label);
                }
                return;
            }
            {
                let mut guard = v.state.borrow_mut();
                let Some(st) = guard.as_mut() else { return };
                match st.pos {
                    ViewPos::Full(n) => {
                        draw_full_page(cr, st, n, w, h, sf);
                    }
                    ViewPos::Split(n) => {
                        // Top: upper half of the next page; bottom: lower half
                        // of the current page, separated by a line.
                        draw_half_page(cr, st, n + 1, w, 0.0, h / 2.0, sf);
                        draw_half_page(cr, st, n, w, h / 2.0, h, sf);
                        cr.set_source_rgb(0.4, 0.4, 0.4);
                        cr.set_line_width(2.0);
                        cr.move_to(0.0, h / 2.0);
                        cr.line_to(w, h / 2.0);
                        let _ = cr.stroke();
                    }
                }
                // Eraser cursor: a circle the size of the erase radius.
                if let Some(pass) = st.erasing.as_ref()
                    && let ViewPos::Full(n) = st.pos
                    && let Some(page) = with_poppler(|| st.doc.page(n as i32))
                {
                    let (pw, ph) = with_poppler(|| page.size());
                    let (scale, _, _) = view_transform(st, w, h, pw, ph);
                    let (cx, cy) = pass.cursor;
                    cr.set_source_rgba(0.3, 0.3, 0.3, 0.9);
                    cr.set_line_width(1.5);
                    cr.arc(cx, cy, ERASER_RADIUS * scale, 0.0, std::f64::consts::TAU);
                    let _ = cr.stroke();
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

    // ----- Input (stylus, finger, mouse, tap to turn) -----

    fn setup_gestures(&self) {
        // Stylus with pressure. The pen always draws – no mode needed – and
        // switches a split view to the full page it touched.
        // The pen tip draws, the pen's eraser end erases – both regardless
        // of the buttons, which only govern finger/mouse.
        let stylus = gtk::GestureStylus::new();
        // Which end is touching (pen tip vs eraser) is carried by the device
        // tool's type. GTK4/Wayland can apply a pen↔eraser tool change a beat
        // late, so the down event may still report the previous tool; the
        // proximity signal fires as the tool nears the screen and updates the
        // flag first. Subscribing to it is also what makes GTK track the tool
        // change at all. The down handler then trusts whichever of the two
        // saw the eraser.
        let er = self.stylus_eraser.clone();
        stylus.connect_proximity(move |g, _, _| er.set(stylus_is_eraser(g)));
        let v = self.clone();
        stylus.connect_down(move |g, x, y| {
            let eraser = stylus_is_eraser(g) || v.stylus_eraser.get();
            let p = g.axis(gtk::gdk::AxisUse::Pressure).unwrap_or(0.5);
            v.pointer_down(x, y, p, true, eraser);
        });
        let v = self.clone();
        stylus.connect_motion(move |g, x, y| {
            let p = g.axis(gtk::gdk::AxisUse::Pressure).unwrap_or(0.5);
            v.pointer_move(x, y, p, true);
        });
        let v = self.clone();
        stylus.connect_up(move |_, x, y| {
            v.pointer_up(x, y, 0.5, true);
        });
        self.area.add_controller(stylus);

        // Finger and mouse. They draw (pen button) or erase (eraser button);
        // the pen is denied here (the stylus gesture handles it). A single
        // finger acts; a second finger starts the zoom gesture below, which
        // aborts the stray stroke. Resting-palm touches are rejected while
        // the stylus is active.
        let drag = gtk::GestureDrag::new();
        drag.set_button(gtk::gdk::BUTTON_PRIMARY);
        let v = self.clone();
        drag.connect_drag_begin(move |g, x, y| {
            use gtk::gdk::InputSource;
            let src = event_source(g.upcast_ref::<gtk::EventController>());
            // The stylus (pen tip and eraser end, both InputSource::Pen) is
            // handled by the stylus gesture above; only finger/mouse act here.
            if matches!(src, Some(InputSource::Pen) | Some(InputSource::TabletPad))
                || !v.finger_active_allowed()
            {
                g.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            v.pointer_down(x, y, 0.5, false, false);
        });
        let v = self.clone();
        drag.connect_drag_update(move |g, dx, dy| {
            if let Some((sx, sy)) = g.start_point() {
                v.pointer_move(sx + dx, sy + dy, 0.5, false);
            }
        });
        let v = self.clone();
        drag.connect_drag_end(move |g, dx, dy| {
            if let Some((sx, sy)) = g.start_point() {
                v.pointer_up(sx + dx, sy + dy, 0.5, false);
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
            let mut guard = v.state.borrow_mut();
            let Some(st) = guard.as_mut() else { return };
            // Palm rejection: no zoom while the stylus draws.
            if st.stylus_active {
                return;
            }
            // A second finger turns a nascent one-finger stroke or erase
            // into a zoom; drop it so nothing gets committed.
            st.current = None;
            st.erasing = None;
            let ViewPos::Full(n) = st.pos else { return };
            let Some(page) = with_poppler(|| st.doc.page(n as i32)) else { return };
            let (pw, ph) = with_poppler(|| page.size());
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
            let Some(page) = with_poppler(|| st.doc.page(n as i32)) else { return };
            let (pw, ph) = with_poppler(|| page.size());
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
            let src = event_source(g.upcast_ref::<gtk::EventController>());
            let Some((editing, zoomed, stylus_active)) = v
                .state
                .borrow()
                .as_ref()
                .map(|st| (st.annotate || st.erase, st.zoom > 1.0, st.stylus_active))
            else {
                return;
            };
            // Palm rejection: ignore touch taps while the stylus draws.
            if stylus_active && src == Some(gtk::gdk::InputSource::Touchscreen) {
                return;
            }
            let w = v.area.width() as f64;
            if (w / 3.0..=w * 2.0 / 3.0).contains(&x) {
                if !editing || src == Some(gtk::gdk::InputSource::Touchscreen) {
                    v.toggle_nav();
                }
                return;
            }
            if editing || zoomed {
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

    /// Whether a finger/mouse drag should act right now (draw or erase mode
    /// on and no stylus stroke in progress).
    fn finger_active_allowed(&self) -> bool {
        self.state
            .borrow()
            .as_ref()
            .map(|st| (st.annotate || st.erase) && !st.stylus_active)
            .unwrap_or(false)
    }

    /// What a pointer contact does. The pen tip always draws and its eraser
    /// end always erases; finger/mouse follow the buttons (and are rejected
    /// as palm while the stylus is active).
    fn action_for(&self, is_pen: bool, is_eraser: bool) -> Action {
        let guard = self.state.borrow();
        let Some(st) = guard.as_ref() else { return Action::Ignore };
        if is_pen {
            return if is_eraser { Action::Erase } else { Action::Draw };
        }
        if st.stylus_active {
            return Action::Ignore;
        }
        if st.erase {
            Action::Erase
        } else if st.annotate {
            Action::Draw
        } else {
            Action::Ignore
        }
    }

    fn pointer_down(&self, x: f64, y: f64, pressure: f64, is_pen: bool, is_eraser: bool) {
        match self.action_for(is_pen, is_eraser) {
            Action::Draw => self.stroke_begin(x, y, pressure, is_pen),
            Action::Erase => self.erase_begin(x, y, is_pen),
            Action::Ignore => {}
        }
    }

    fn pointer_move(&self, x: f64, y: f64, pressure: f64, is_pen: bool) {
        let erasing = self
            .state
            .borrow()
            .as_ref()
            .is_some_and(|st| st.erasing.is_some());
        if erasing {
            self.erase_move(x, y, is_pen);
        } else {
            self.stroke_move(x, y, pressure, is_pen);
        }
    }

    fn pointer_up(&self, x: f64, y: f64, pressure: f64, is_pen: bool) {
        let erasing = self
            .state
            .borrow()
            .as_ref()
            .is_some_and(|st| st.erasing.is_some());
        if erasing {
            self.erase_end(is_pen);
        } else {
            self.stroke_end(x, y, pressure, is_pen);
        }
    }

    /// Starts a stroke. `is_pen` marks stylus input, which draws in any
    /// mode; finger/mouse input has already been gated by the caller. On a
    /// split view the contact picks the page half to switch to full.
    fn stroke_begin(&self, x: f64, y: f64, pressure: f64, is_pen: bool) {
        let mut switched = false;
        {
            let mut guard = self.state.borrow_mut();
            let Some(st) = guard.as_mut() else { return };
            if !is_pen && (!st.annotate || st.stylus_active) {
                return;
            }
            if is_pen {
                st.stylus_active = true;
            }
            if let ViewPos::Split(n) = st.pos {
                let h = self.area.height() as f64;
                st.pos = if y < h / 2.0 {
                    ViewPos::Full(n + 1)
                } else {
                    ViewPos::Full(n)
                };
                reset_zoom(st);
                switched = true;
            }
            if let Some(pt) = self.widget_to_page(st, x, y, pressure) {
                st.current = Some(Stroke {
                    points: vec![pt],
                    color: self.cfg.pen_rgb(),
                    width: self.cfg.pen_width,
                });
            }
        }
        if switched {
            self.update_status();
        }
        self.area.queue_draw();
    }

    fn stroke_move(&self, x: f64, y: f64, pressure: f64, is_pen: bool) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        if st.current.is_none() || (!is_pen && st.stylus_active) {
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

    fn stroke_end(&self, x: f64, y: f64, pressure: f64, is_pen: bool) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        if is_pen {
            st.stylus_active = false;
        } else if st.stylus_active {
            // A palm finger lifting while the stylus draws: not our stroke.
            return;
        }
        let Some(mut cur) = st.current.take() else {
            drop(guard);
            self.area.queue_draw();
            return;
        };
        if let Some(pt) = self.widget_to_page(st, x, y, pressure) {
            cur.points.push(pt);
        }
        let page = st.pos.base_page();
        let record = cur.clone();
        let v = st.strokes.entry(page).or_default();
        let idx = v.len();
        v.push(cur);
        // Drawing appends at the end – record it and drop any redo history.
        st.undo.entry(page).or_default().push(Edit::Added(idx, record));
        st.redo.remove(&page);
        drop(guard);
        self.arm_autosave();
        self.update_history();
        self.area.queue_draw();
    }

    // ----- Erasing (partial: trims the strokes along the eraser path) -----

    /// Begins an erase pass, switching a split view to the touched full page
    /// (like drawing). `is_pen` marks the stylus' eraser end.
    fn erase_begin(&self, x: f64, y: f64, is_pen: bool) {
        {
            let mut guard = self.state.borrow_mut();
            let Some(st) = guard.as_mut() else { return };
            if is_pen {
                st.stylus_active = true;
            }
            if let ViewPos::Split(n) = st.pos {
                let h = self.area.height() as f64;
                st.pos = if y < h / 2.0 {
                    ViewPos::Full(n + 1)
                } else {
                    ViewPos::Full(n)
                };
                reset_zoom(st);
            }
            st.erasing = Some(ErasePass {
                page: st.pos.base_page(),
                ops: Vec::new(),
                cursor: (x, y),
            });
        }
        self.update_status();
        self.erase_hit(x, y);
    }

    fn erase_move(&self, x: f64, y: f64, is_pen: bool) {
        {
            let guard = self.state.borrow();
            let Some(st) = guard.as_ref() else { return };
            if st.erasing.is_none() || (!is_pen && st.stylus_active) {
                return;
            }
        }
        self.erase_hit(x, y);
    }

    /// Removes every stroke under the eraser at widget point (x, y).
    fn erase_hit(&self, x: f64, y: f64) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        let Some(page) = st.erasing.as_ref().map(|p| p.page) else { return };
        // Move the cursor even when the point is off-page.
        let pt = self.widget_to_page(st, x, y, 0.5);
        if let Some(p) = st.erasing.as_mut() {
            p.cursor = (x, y);
        }
        let Some(pt) = pt else {
            drop(guard);
            self.area.queue_draw();
            return;
        };
        // Trim from the highest index down, so splicing in a stroke's pieces
        // never shifts an index still to be processed. Record each trim in
        // that order; undo reverses the whole pass in reverse.
        let mut ops = Vec::new();
        if let Some(strokes) = st.strokes.get_mut(&page) {
            let mut i = strokes.len();
            while i > 0 {
                i -= 1;
                let Some(pieces) = trim_stroke(&strokes[i], pt.x, pt.y, ERASER_RADIUS) else {
                    continue;
                };
                let original = strokes.remove(i);
                for (k, piece) in pieces.iter().enumerate() {
                    strokes.insert(i + k, piece.clone());
                }
                ops.push(TrimOp { index: i, original, pieces });
            }
        }
        let hit = !ops.is_empty();
        if let Some(p) = st.erasing.as_mut() {
            p.ops.extend(ops);
        }
        drop(guard);
        if hit {
            self.area.queue_draw();
        }
    }

    /// Finishes an erase pass, turning its trims into one undo step.
    fn erase_end(&self, is_pen: bool) {
        let mut guard = self.state.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        if is_pen {
            st.stylus_active = false;
        }
        let Some(pass) = st.erasing.take() else {
            drop(guard);
            self.area.queue_draw();
            return;
        };
        let erased = !pass.ops.is_empty();
        if erased {
            st.undo
                .entry(pass.page)
                .or_default()
                .push(Edit::Trimmed(pass.ops));
            st.redo.remove(&pass.page);
            st.rewrite.insert(pass.page);
        }
        drop(guard);
        if erased {
            self.arm_autosave();
            self.update_history();
        }
        self.area.queue_draw();
    }

    /// Widget coordinates → page coordinates. Only meaningful in Full mode,
    /// which drawing switches to via [`stroke_begin`].
    fn widget_to_page(&self, st: &DocState, x: f64, y: f64, pressure: f64) -> Option<StrokePoint> {
        let ViewPos::Full(n) = st.pos else { return None };
        let page = with_poppler(|| st.doc.page(n as i32))?;
        let (pw, ph) = with_poppler(|| page.size());
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

/// Draws the placeholder page for a missing/broken setlist entry: a heading
/// and the entry's label, centred on the paper-white area.
fn draw_placeholder(cr: &cairo::Context, w: f64, h: f64, label: &str) {
    cr.set_source_rgb(0.5, 0.5, 0.5);
    cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    let centered = |text: &str, size: f64, y: f64| {
        cr.set_font_size(size);
        if let Ok(ext) = cr.text_extents(text) {
            cr.move_to((w - ext.width()) / 2.0 - ext.x_bearing(), y);
            let _ = cr.show_text(text);
        }
    };
    centered("Nicht gefunden:", 26.0, h / 2.0 - 12.0);
    centered(label, 20.0, h / 2.0 + 22.0);
}

/// Whether a view position is the last page of the range (`hi`) – the point
/// where turning forward crosses into the next setlist piece. A split view is
/// never the last page (its bottom half is not the final one).
fn pos_at_end(pos: ViewPos, hi: usize) -> bool {
    pos == ViewPos::Full(hi)
}

/// Whether a view position is the first page of the range (`lo`) – where
/// turning back crosses into the previous piece.
fn pos_at_start(pos: ViewPos, lo: usize) -> bool {
    pos == ViewPos::Full(lo)
}

/// Resolves a 1-based, inclusive page range against a document's page count
/// into 0-based `(lo, hi)` bounds, clamped to the document. An open side falls
/// back to the first/last page; `None` means the whole document. A range wholly
/// past the end clamps to the last page (Stufe 5 turns this into a placeholder).
fn bounds_for(range: Option<PageRange>, n_pages: usize) -> (usize, usize) {
    let last = n_pages.saturating_sub(1);
    match range {
        None => (0, last),
        Some(r) => {
            let lo = r.lo.map(|n| n.saturating_sub(1)).unwrap_or(0).min(last);
            let hi = r.hi.map(|n| n.saturating_sub(1)).unwrap_or(last).min(last);
            (lo, hi.max(lo))
        }
    }
}

/// Whether a range starts past the last page, so none of it is in the file –
/// treated as broken (placeholder page), not clamped.
fn range_past_end(range: Option<PageRange>, n_pages: usize) -> bool {
    matches!(range, Some(r) if r.lo.is_some_and(|l| l > n_pages))
}

/// The neighbouring playlist index in direction `dir` (+1 / -1), or `None` at
/// the ends (so turning clamps instead of wrapping).
fn step_index(index: usize, len: usize, dir: i32) -> Option<usize> {
    let ni = index as i32 + dir;
    if ni < 0 || ni as usize >= len {
        None
    } else {
        Some(ni as usize)
    }
}

/// Loads the document for rendering (Poppler) together with its strokes.
/// The ink annotations are stripped from the copy Poppler sees – frack
/// draws them itself – so they never render twice.
/// Poppler has process-global state and is not thread-safe, so every call into
/// it – parsing a document and rendering pages, on the main thread and on the
/// thumbnail workers alike – is serialized here. Only one thread is ever inside
/// Poppler at a time, which is what prevents the heap corruption a concurrent
/// render otherwise causes. The lock wraps a single leaf call each time, so it
/// can never deadlock by re-entrancy. A panic while rendering does not corrupt
/// Rust state, so the poison is recovered rather than propagated.
fn with_poppler<T>(f: impl FnOnce() -> T) -> T {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

fn load_document(path: &Path) -> Result<(poppler::Document, StrokesByPage), String> {
    let (bytes, strokes) = annot::load_and_strip(path).map_err(|e| e.to_string())?;
    let gbytes = glib::Bytes::from_owned(bytes);
    let doc = with_poppler(|| poppler::Document::from_bytes(&gbytes, None)).map_err(|e| e.to_string())?;
    Ok((doc, strokes))
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
    let Some(page) = with_poppler(|| st.doc.page(n as i32)) else { return true };
    let (pw, ph) = with_poppler(|| page.size());
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
    let Some(page) = with_poppler(|| st.doc.page(n as i32)) else { return };
    let (pw, ph) = with_poppler(|| page.size());
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
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let _ = cr.paint();
        cr.translate(ox, oy);
        cr.scale(scale, scale);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.rectangle(0.0, 0.0, pw, ph);
        let _ = cr.fill();
        with_poppler(|| page.render(&cr));
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
    let page = with_poppler(|| st.doc.page(n as i32))?;
    let (pw, ph) = with_poppler(|| page.size());
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

/// Thumbnail worker thread: opens its own Poppler document and renders queued
/// pages until the queue is empty or quit is set. Every Poppler call (here and
/// on the main thread) goes through [`with_poppler`], so no two threads are
/// ever inside Poppler at once.
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
        .and_then(|uri| with_poppler(|| poppler::Document::from_file(uri.as_str(), None)).ok());
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
    let page = with_poppler(|| doc.page(n as i32))?;
    let (pw, ph) = with_poppler(|| page.size());
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
        with_poppler(|| page.render(&cr));
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
    let Some(page) = with_poppler(|| st.doc.page(n as i32)) else { return };
    let (pw, ph) = with_poppler(|| page.size());
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
        with_poppler(|| page.render(&cr));
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

fn draw_full_page(cr: &cairo::Context, st: &mut DocState, n: usize, w: f64, h: f64, sf: f64) {
    let Some(page) = with_poppler(|| st.doc.page(n as i32)) else { return };
    let (pw, ph) = with_poppler(|| page.size());
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
    draw_strokes(cr, st, n, true);
    cr.restore().ok();
}

/// Draws page `n` clipped to the region y0..y1 of the drawing area,
/// positioned exactly as the centered full-page view would place it. With
/// the two regions being the top and bottom half of the area, the split at
/// region_h lands on the page's vertical midline, so completing a
/// half-page turn (split → full) does not shift the page.
#[allow(clippy::too_many_arguments)]
fn draw_half_page(
    cr: &cairo::Context,
    st: &mut DocState,
    n: usize,
    w: f64,
    y0: f64,
    y1: f64,
    sf: f64,
) {
    let Some(page) = with_poppler(|| st.doc.page(n as i32)) else { return };
    let (pw, ph) = with_poppler(|| page.size());
    let region_h = y1 - y0;
    // Half a page in half the area uses the same fit as a full page in the
    // full area (height 2·region_h), so the cached full-view bitmap fits
    // and the page lands at the identical position as draw_full_page.
    let full_h = 2.0 * region_h;
    let (scale, ox, oy) = full_layout(w, full_h, pw, ph);
    ensure_cached(st, n, w, full_h, sf);
    cr.save().ok();
    cr.rectangle(0.0, y0, w, region_h);
    cr.clip();
    if let Some(cached) = st.cache.get(&n) {
        blit_page(cr, cached, ox, oy, sf);
    }
    cr.translate(ox, oy);
    cr.scale(scale, scale);
    draw_strokes(cr, st, n, false);
    cr.restore().ok();
}

/// Draws a page's strokes (the overlay that is frack's source of truth);
/// the cairo coordinate system must already be the page's. Each stroke uses
/// its own colour and width, so strokes drawn elsewhere keep their look.
fn draw_strokes(cr: &cairo::Context, st: &DocState, page_idx: usize, include_current: bool) {
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);

    let saved = st.strokes.get(&page_idx).map(|v| v.as_slice()).unwrap_or(&[]);
    let current = if include_current && st.pos.base_page() == page_idx {
        st.current.as_ref()
    } else {
        None
    };
    for stroke in saved.iter().chain(current) {
        let (r, g, b) = stroke.color;
        cr.set_source_rgb(r, g, b);
        match stroke.points.len() {
            0 => {}
            1 => {
                let p = &stroke.points[0];
                cr.set_line_width(annot::width_for(stroke.width, p.pressure));
                cr.move_to(p.x, p.y);
                cr.line_to(p.x, p.y);
                let _ = cr.stroke();
            }
            _ => {
                for pair in stroke.points.windows(2) {
                    let w = annot::width_for(
                        stroke.width,
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

/// True when the stylus' current contact is the eraser end. GTK4 no longer
/// has a separate `InputSource::Eraser` (it was removed in the 3→4 port);
/// the eraser is told apart only by the device tool's type. The tool is read
/// from the gesture and, as a fallback, from the current event (drivers
/// differ on where it shows up).
fn stylus_is_eraser(g: &gtk::GestureStylus) -> bool {
    let event = g.upcast_ref::<gtk::EventController>().current_event();
    let tool = g
        .device_tool()
        .or_else(|| event.as_ref().and_then(|e| e.device_tool()));
    tool.as_ref().map(|t| t.tool_type()) == Some(gtk::gdk::DeviceToolType::Eraser)
}

/// Trims a stroke against an eraser disc (centre (cx, cy), radius r in page
/// points): the parts of the polyline inside the disc are removed, splitting
/// it into the remaining pieces (each a stroke of the same colour and width).
/// Returns `None` when the disc does not touch the stroke (no change), or
/// `Some(pieces)` otherwise (empty when the whole stroke is erased).
fn trim_stroke(s: &Stroke, cx: f64, cy: f64, r: f64) -> Option<Vec<Stroke>> {
    let r2 = r * r;
    let inside = |p: &StrokePoint| {
        let (dx, dy) = (p.x - cx, p.y - cy);
        dx * dx + dy * dy <= r2
    };
    match s.points.as_slice() {
        [] => None,
        [p] => inside(p).then(Vec::new),
        pts => {
            let mut pieces: Vec<Vec<StrokePoint>> = Vec::new();
            let mut cur: Vec<StrokePoint> = Vec::new();
            let mut changed = false;
            if inside(&pts[0]) {
                changed = true;
            } else {
                cur.push(pts[0].clone());
            }
            for w in pts.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                // Where the segment crosses the circle: roots of the
                // quadratic |a + t(b-a) - c|^2 = r^2 in (0, 1).
                let (dx, dy) = (b.x - a.x, b.y - a.y);
                let qa = dx * dx + dy * dy;
                let qb = 2.0 * ((a.x - cx) * dx + (a.y - cy) * dy);
                let qc = (a.x - cx).powi(2) + (a.y - cy).powi(2) - r2;
                let mut ts: Vec<f64> = Vec::new();
                if qa > 1e-12 {
                    let disc = qb * qb - 4.0 * qa * qc;
                    if disc > 0.0 {
                        let sq = disc.sqrt();
                        for t in [(-qb - sq) / (2.0 * qa), (-qb + sq) / (2.0 * qa)] {
                            if t > 1e-9 && t < 1.0 - 1e-9 {
                                ts.push(t);
                            }
                        }
                    }
                }
                ts.sort_by(|x, y| x.partial_cmp(y).unwrap());
                let mut prev_inside = inside(a);
                for t in ts {
                    changed = true;
                    let cp = lerp_point(a, b, t);
                    if prev_inside {
                        // Leaving the disc: start a fresh piece.
                        cur = vec![cp];
                    } else {
                        // Entering the disc: close off the current piece.
                        cur.push(cp);
                        pieces.push(std::mem::take(&mut cur));
                    }
                    prev_inside = !prev_inside;
                }
                if inside(b) {
                    changed = true;
                } else {
                    cur.push(b.clone());
                }
            }
            if !cur.is_empty() {
                pieces.push(cur);
            }
            changed.then(|| {
                pieces
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .map(|points| Stroke { points, color: s.color, width: s.width })
                    .collect()
            })
        }
    }
}

/// Linear interpolation between two stroke points (position and pressure).
fn lerp_point(a: &StrokePoint, b: &StrokePoint, t: f64) -> StrokePoint {
    StrokePoint {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
        pressure: a.pressure + (b.pressure - a.pressure) * t,
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

    #[test]
    fn pos_at_end_only_on_the_last_page_of_the_range() {
        assert!(pos_at_end(ViewPos::Full(1), 1)); // hi = 1
        assert!(!pos_at_end(ViewPos::Full(0), 1));
        // A split view shows the bottom of page n, never the last page.
        assert!(!pos_at_end(ViewPos::Split(0), 1));
    }

    #[test]
    fn pos_at_start_only_on_the_first_page_of_the_range() {
        // A ranged excerpt starting at page 3 (lo = 2).
        assert!(pos_at_start(ViewPos::Full(2), 2));
        assert!(!pos_at_start(ViewPos::Full(0), 2));
        assert!(!pos_at_start(ViewPos::Split(2), 2));
    }

    #[test]
    fn step_index_clamps_at_the_ends() {
        assert_eq!(step_index(0, 3, 1), Some(1));
        assert_eq!(step_index(2, 3, 1), None); // no piece past the last
        assert_eq!(step_index(1, 3, -1), Some(0));
        assert_eq!(step_index(0, 3, -1), None); // no piece before the first
    }

    #[test]
    fn bounds_for_resolves_and_clamps_ranges() {
        let r = |lo, hi| Some(PageRange { lo, hi });
        assert_eq!(bounds_for(None, 3), (0, 2)); // whole document
        assert_eq!(bounds_for(r(Some(2), Some(2)), 3), (1, 1)); // single page 2
        assert_eq!(bounds_for(r(Some(1), Some(2)), 3), (0, 1)); // 1-2
        assert_eq!(bounds_for(r(Some(2), None), 3), (1, 2)); // 2- (to end)
        assert_eq!(bounds_for(r(None, Some(2)), 3), (0, 1)); // -2 (from start)
        // Wholly past the end: clamps to the last page (but flagged broken
        // below, so it becomes a placeholder rather than showing that page).
        assert_eq!(bounds_for(r(Some(50), None), 2), (1, 1));
    }

    #[test]
    fn range_past_end_detects_out_of_bounds_starts() {
        let r = |lo, hi| Some(PageRange { lo, hi });
        assert!(range_past_end(r(Some(50), None), 2)); // starts at 50 in 2 pages
        assert!(range_past_end(r(Some(3), Some(5)), 2));
        assert!(!range_past_end(r(Some(2), None), 2)); // last page, still inside
        assert!(!range_past_end(r(None, Some(50)), 2)); // open start clamps, inside
        assert!(!range_past_end(None, 2));
    }

    /// Renders the given view position into a PNG, exactly like the
    /// draw func does, so split rendering can be checked headlessly.
    fn render_view(st: &mut DocState, w: f64, h: f64, out: &Path) {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32).unwrap();
        {
            let cr = cairo::Context::new(&surface).unwrap();
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.paint().unwrap();
            match st.pos {
                ViewPos::Full(n) => draw_full_page(&cr, st, n, w, h, 1.0),
                ViewPos::Split(n) => {
                    draw_half_page(&cr, st, n + 1, w, 0.0, h / 2.0, 1.0);
                    draw_half_page(&cr, st, n, w, h / 2.0, h, 1.0);
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
        let (doc, strokes) = load_document(&pdf).unwrap();
        let saved = strokes.iter().map(|(&p, v)| (p, v.len())).collect();
        let mut st = DocState {
            path: pdf.clone(),
            doc,
            n_pages: 2,
            lo: 0,
            hi: 1,
            pos: ViewPos::Split(0),
            annotate: false,
            erase: false,
            strokes,
            saved,
            rewrite: HashSet::new(),
            undo: BTreeMap::new(),
            redo: BTreeMap::new(),
            current: None,
            erasing: None,
            stylus_active: false,
            cache: BTreeMap::new(),
            thumbs: BTreeMap::new(),
            zoom: 1.0,
            view: (0.0, 0.0),
            zoom_cache: None,
        };
        let (w, h) = (300.0, 400.0);
        let out = if let Ok(d) = std::env::var("FRACK_TEST_OUT") {
            PathBuf::from(d).join("split.png")
        } else {
            dir.join("split.png")
        };
        render_view(&mut st, w, h, &out);

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

    fn stroke(points: Vec<StrokePoint>, width: f64) -> Stroke {
        Stroke { points, color: (0.0, 0.0, 0.0), width }
    }

    fn pt(x: f64, y: f64) -> StrokePoint {
        StrokePoint { x, y, pressure: 0.5 }
    }

    #[test]
    fn trim_splits_a_stroke_where_the_eraser_crosses() {
        // Horizontal line y=100 from x=0..200 (radius 10 → cuts x in [90,110]).
        let s = stroke(vec![pt(0.0, 100.0), pt(200.0, 100.0)], 2.0);
        let pieces = trim_stroke(&s, 100.0, 100.0, ERASER_RADIUS).expect("should trim");
        assert_eq!(pieces.len(), 2, "erasing the middle splits into two pieces");
        let left_end = pieces[0].points.last().unwrap().x;
        let right_start = pieces[1].points[0].x;
        assert!((left_end - 90.0).abs() < 0.5, "left ends at {left_end}");
        assert!((right_start - 110.0).abs() < 0.5, "right starts at {right_start}");
        // Colour and width carry over to the pieces.
        assert_eq!(pieces[0].width, s.width);

        // Eraser far from the line: no change.
        assert!(trim_stroke(&s, 100.0, 130.0, ERASER_RADIUS).is_none());

        // A dot inside the eraser is erased entirely; outside it is untouched.
        let dot = stroke(vec![pt(50.0, 50.0)], 1.0);
        assert_eq!(trim_stroke(&dot, 52.0, 50.0, ERASER_RADIUS).unwrap().len(), 0);
        assert!(trim_stroke(&dot, 70.0, 50.0, ERASER_RADIUS).is_none());

        // Erasing one end leaves a single shortened piece.
        let one = trim_stroke(&s, 0.0, 100.0, ERASER_RADIUS).expect("should trim");
        assert_eq!(one.len(), 1);
        assert!((one[0].points[0].x - 10.0).abs() < 0.5, "starts at {}", one[0].points[0].x);
    }
}
