// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

use frack::{config, library, setlist, tuner, viewer};

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Maximum number of setlists shown in the library while the search is empty.
/// Older ones stay reachable by typing; recency keeps the view uncluttered.
const RECENT_SETLISTS: usize = 6;

/// A row in the library list, carrying an index into the setlists or scores
/// vector (by list position). Section headers are not rows – they are drawn
/// by the list's header function, so they never take focus or an index.
enum LibRow {
    Setlist(usize),
    Score(usize),
}

/// A row in the setlist view: a `#` line rendered as a section header, or an
/// entry carrying its index into the resolved-entries vector.
enum SlRow {
    Section,
    Entry(usize),
}

/// The setlist currently open in the setlist view / being played, kept so the
/// viewer's back button returns to it and turning can cross pieces.
struct Active {
    setlist: setlist::Setlist,
    resolved: Vec<setlist::Resolved>,
    /// Index (into the entries, comments skipped) of the piece being played,
    /// or `None` before one is opened. Highlighted in the setlist view.
    index: Option<usize>,
}

/// Case-insensitive: all whitespace-separated terms occur in the name.
fn setlist_matches(name: &str, query: &str) -> bool {
    let hay = name.to_lowercase();
    query
        .split_whitespace()
        .all(|t| hay.contains(&t.to_lowercase()))
}

/// Converts a parsed setlist range into the viewer's range. An invalid range
/// falls back to the whole file for now (Stufe 5 turns it into a placeholder).
fn to_viewer_range(range: Option<setlist::PageRange>) -> Option<viewer::PageRange> {
    match range {
        Some(setlist::PageRange::Valid { lo, hi }) => Some(viewer::PageRange { lo, hi }),
        _ => None,
    }
}

/// A short "(S. …)" hint for a page range, for display in the setlist view.
fn range_hint(range: &Option<setlist::PageRange>) -> String {
    match range {
        Some(setlist::PageRange::Valid { lo, hi }) => match (lo, hi) {
            (Some(a), Some(b)) if a == b => format!("  (S. {a})"),
            (Some(a), Some(b)) => format!("  (S. {a}–{b})"),
            (Some(a), None) => format!("  (S. {a}–)"),
            (None, Some(b)) => format!("  (S. –{b})"),
            (None, None) => String::new(),
        },
        Some(setlist::PageRange::Invalid) => "  (Seiten?)".to_string(),
        None => String::new(),
    }
}

/// A non-activatable section header row for a `gtk::ListBox`.
fn header_row(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::new(None);
    label.set_markup(&format!("<b>{text}</b>"));
    label.set_xalign(0.0);
    label.set_margin_top(10);
    label.set_margin_bottom(4);
    label.set_margin_start(8);
    label.set_margin_end(8);
    label.add_css_class("dim-label");
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&label));
    row.set_activatable(false);
    row.set_selectable(false);
    // Not focusable, so keyboard/pedal cursor movement skips the header and
    // lands on the first real item.
    row.set_focusable(false);
    row
}

/// A section-header label for a `gtk::ListBox` header function. Unlike a row,
/// a header never takes focus and is not counted in `row.index()`.
fn section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_markup(&format!("<b>{text}</b>"));
    label.set_xalign(0.0);
    label.set_margin_top(10);
    label.set_margin_bottom(4);
    label.set_margin_start(8);
    label.set_margin_end(8);
    label.add_css_class("dim-label");
    label
}

/// An activatable list row whose child is a left-aligned label with `text`.
/// A themed symbolic icon (by name, e.g. `view-list-symbolic`) is prepended
/// when given – symbolic icons render everywhere the icon theme is present,
/// unlike emoji, which need a colour-emoji font the screenshot VM lacks.
fn item_row(text: &str, icon: Option<&str>) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(8);
    content.set_margin_end(8);
    if let Some(icon) = icon {
        content.append(&gtk::Image::from_icon_name(icon));
    }
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    content.append(&label);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));
    row
}

fn main() -> glib::ExitCode {
    // Optional library directory as the only argument; it overrides
    // root_dir from the config for this run (nothing is written to the
    // config). Handy for e.g. `frack sample-scores`.
    let mut args = std::env::args().skip(1);
    let root_override = match args.next().as_deref() {
        Some("-h" | "--help") => {
            println!("Usage: frack [LIBRARY_DIR]");
            println!();
            println!("LIBRARY_DIR overrides root_dir from the config for this run.");
            return glib::ExitCode::SUCCESS;
        }
        Some(dir) => {
            // Canonicalize so relative paths survive whatever GTK does
            // with the working directory.
            Some(std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir)))
        }
        None => None,
    };

    let app = gtk::Application::builder()
        .application_id("app.frack.Frack")
        .build();
    app.connect_activate(move |app| build_ui(app, root_override.clone()));
    // GTK must not parse our command line (it would reject LIBRARY_DIR).
    app.run_with_args::<String>(&[])
}

fn build_ui(app: &gtk::Application, root_override: Option<PathBuf>) {
    let (mut cfg, cfg_created) = config::load_or_create();
    if let Some(dir) = root_override {
        cfg.root_dir = dir;
    }
    let cfg = Rc::new(cfg);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Frack")
        .icon_name("app.frack.Frack")
        .default_width(850)
        .default_height(1100)
        .build();
    // Ensures the icon is used even without an installed desktop file.
    gtk::Window::set_default_icon_name("app.frack.Frack");

    // ----- Header bar -----
    let header = gtk::HeaderBar::new();
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some("Zur Bibliothek (Esc)"));
    back.set_visible(false);
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Neu einlesen"));
    let pen = gtk::ToggleButton::new();
    pen.set_icon_name("document-edit-symbolic");
    pen.set_tooltip_text(Some("Anmerkungsmodus (a)"));
    pen.set_visible(false);
    let erase = gtk::ToggleButton::new();
    erase.set_icon_name("error-correct-symbolic");
    erase.set_tooltip_text(Some("Radiergummi (e)"));
    erase.set_visible(false);
    let undo = gtk::Button::from_icon_name("edit-undo-symbolic");
    undo.set_tooltip_text(Some("Strich zurücknehmen (Strg+Z)"));
    undo.set_visible(false);
    let redo = gtk::Button::from_icon_name("edit-redo-symbolic");
    redo.set_tooltip_text(Some("Strich wiederherstellen (Strg+Umschalt+Z)"));
    redo.set_visible(false);
    let tuner_btn = gtk::ToggleButton::new();
    tuner_btn.set_icon_name("audio-input-microphone-symbolic");
    tuner_btn.set_tooltip_text(Some("Stimmgerät (t)"));
    // The header bar is only visible outside fullscreen, so this is
    // effectively the touch button for entering fullscreen.
    let header_fullscreen = gtk::Button::from_icon_name("view-fullscreen-symbolic");
    header_fullscreen.set_tooltip_text(Some("Vollbild (F11)"));
    let status = gtk::Label::new(Some("Bibliothek"));
    status.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    header.pack_start(&back);
    header.pack_start(&refresh);
    header.pack_start(&tuner_btn);
    header.pack_end(&header_fullscreen);
    header.pack_end(&pen);
    header.pack_end(&erase);
    header.pack_end(&undo);
    header.pack_end(&redo);
    header.set_title_widget(Some(&status));
    window.set_titlebar(Some(&header));

    // ----- Library -----
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Suchen …"));
    let info = gtk::Label::new(None);
    info.set_wrap(true);
    info.set_xalign(0.0);
    info.set_visible(false);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_child(Some(&list));
    scrolled.set_vexpand(true);
    // In fullscreen the header bar is hidden; this button (only visible
    // then) keeps the library usable without a keyboard.
    let lib_unfullscreen = gtk::Button::from_icon_name("view-restore-symbolic");
    lib_unfullscreen.set_tooltip_text(Some("Vollbild verlassen"));
    let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    search.set_hexpand(true);
    search_row.append(&search);
    search_row.append(&lib_unfullscreen);
    let libbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    libbox.set_margin_top(6);
    libbox.set_margin_bottom(6);
    libbox.set_margin_start(6);
    libbox.set_margin_end(6);
    libbox.append(&search_row);
    libbox.append(&info);
    libbox.append(&scrolled);
    window
        .bind_property("fullscreened", &lib_unfullscreen, "visible")
        .sync_create()
        .build();
    {
        let window = window.clone();
        lib_unfullscreen.connect_clicked(move |_| window.unfullscreen());
    }
    {
        let window = window.clone();
        header_fullscreen.connect_clicked(move |_| window.fullscreen());
    }

    // ----- Viewer -----
    let viewer = viewer::Viewer::new(cfg.clone(), status.clone(), pen.clone(), erase.clone());
    viewer.register_undo_button(&undo);
    viewer.register_redo_button(&redo);

    let stack = gtk::Stack::new();
    stack.add_named(&libbox, Some("library"));
    stack.add_named(viewer.widget(), Some("viewer"));

    // Tuner bar at the very top, above library and viewer alike.
    let tuner = tuner::Tuner::new(cfg.clone());
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(tuner.widget());
    root.append(&stack);
    window.set_child(Some(&root));

    {
        let tuner = tuner.clone();
        let status = status.clone();
        tuner_btn.connect_toggled(move |btn| {
            if btn.is_active() {
                if let Err(e) = tuner.start() {
                    status.set_text(&format!("Stimmgerät: {e}"));
                    btn.set_active(false);
                }
            } else {
                tuner.stop();
            }
        });
    }

    // ----- Setlist view (third stack page) -----
    let sl_list = gtk::ListBox::new();
    // Single selection so the piece currently playing can be highlighted.
    sl_list.set_selection_mode(gtk::SelectionMode::Single);
    let sl_scrolled = gtk::ScrolledWindow::new();
    sl_scrolled.set_child(Some(&sl_list));
    sl_scrolled.set_vexpand(true);
    let sl_back = gtk::Button::from_icon_name("go-previous-symbolic");
    sl_back.set_tooltip_text(Some("Zur Bibliothek"));
    let sl_title = gtk::Label::new(None);
    sl_title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    sl_title.set_xalign(0.0);
    sl_title.set_hexpand(true);
    let sl_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    sl_header.append(&sl_back);
    sl_header.append(&sl_title);
    let slbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    slbox.set_margin_top(6);
    slbox.set_margin_bottom(6);
    slbox.set_margin_start(6);
    slbox.set_margin_end(6);
    slbox.append(&sl_header);
    slbox.append(&sl_scrolled);
    stack.add_named(&slbox, Some("setlist"));
    // In fullscreen the header bar (with its back button) is hidden; the
    // in-page back button then takes over, mirroring the library.
    window
        .bind_property("fullscreened", &sl_back, "visible")
        .sync_create()
        .build();

    // ----- Shared library / setlist state -----
    let setlists: Rc<RefCell<Vec<setlist::SetlistFile>>> = Rc::new(RefCell::new(Vec::new()));
    let scores: Rc<RefCell<Vec<library::Entry>>> = Rc::new(RefCell::new(Vec::new()));
    let rows: Rc<RefCell<Vec<LibRow>>> = Rc::new(RefCell::new(Vec::new()));
    let sl_rows: Rc<RefCell<Vec<SlRow>>> = Rc::new(RefCell::new(Vec::new()));
    let active: Rc<RefCell<Option<Active>>> = Rc::new(RefCell::new(None));

    // Show the viewer chrome (used when opening from the library or a setlist).
    let enter_viewer: Rc<dyn Fn()> = {
        let stack = stack.clone();
        let back = back.clone();
        let pen = pen.clone();
        let erase = erase.clone();
        let undo = undo.clone();
        let redo = redo.clone();
        let refresh = refresh.clone();
        Rc::new(move || {
            stack.set_visible_child_name("viewer");
            back.set_visible(true);
            pen.set_visible(true);
            erase.set_visible(true);
            undo.set_visible(true);
            redo.set_visible(true);
            refresh.set_visible(false);
        })
    };

    let show_library: Rc<dyn Fn()> = {
        let viewer = viewer.clone();
        let stack = stack.clone();
        let back = back.clone();
        let refresh = refresh.clone();
        let pen = pen.clone();
        let erase = erase.clone();
        let undo = undo.clone();
        let redo = redo.clone();
        let status = status.clone();
        let search = search.clone();
        let active = active.clone();
        Rc::new(move || {
            viewer.close();
            active.borrow_mut().take();
            stack.set_visible_child_name("library");
            back.set_visible(false);
            pen.set_visible(false);
            erase.set_visible(false);
            undo.set_visible(false);
            redo.set_visible(false);
            refresh.set_visible(true);
            status.set_text("Bibliothek");
            search.grab_focus();
        })
    };

    // Populate the setlist page from the active setlist and switch to it.
    let show_setlist_view: Rc<dyn Fn()> = {
        let viewer = viewer.clone();
        let stack = stack.clone();
        let back = back.clone();
        let pen = pen.clone();
        let erase = erase.clone();
        let undo = undo.clone();
        let redo = redo.clone();
        let refresh = refresh.clone();
        let status = status.clone();
        let sl_list = sl_list.clone();
        let sl_title = sl_title.clone();
        let sl_rows = sl_rows.clone();
        let active = active.clone();
        Rc::new(move || {
            viewer.close();
            while let Some(row) = sl_list.first_child() {
                sl_list.remove(&row);
            }
            let mut model: Vec<SlRow> = Vec::new();
            let mut current_row: Option<gtk::ListBoxRow> = None;
            if let Some(a) = active.borrow().as_ref() {
                sl_title.set_text(&a.setlist.name);
                status.set_text(&a.setlist.name);
                let mut ei = 0usize;
                for line in &a.setlist.lines {
                    match line {
                        setlist::Line::Comment(s) => {
                            model.push(SlRow::Section);
                            sl_list.append(&header_row(s));
                        }
                        setlist::Line::Entry(_) => {
                            let r = &a.resolved[ei];
                            let text = format!("{}{}", r.rel, range_hint(&r.range));
                            let row = item_row(&text, Some("audio-x-generic-symbolic"));
                            // Missing entries are inert for now; Stufe 5 adds
                            // the placeholder page you can turn onto.
                            row.set_sensitive(r.exists);
                            sl_list.append(&row);
                            if a.index == Some(ei) {
                                current_row = Some(row);
                            }
                            model.push(SlRow::Entry(ei));
                            ei += 1;
                        }
                    }
                }
            }
            *sl_rows.borrow_mut() = model;
            // Highlight the piece currently playing (if any).
            match &current_row {
                Some(r) => sl_list.select_row(Some(r)),
                None => sl_list.unselect_all(),
            }
            stack.set_visible_child_name("setlist");
            back.set_visible(true);
            pen.set_visible(false);
            erase.set_visible(false);
            undo.set_visible(false);
            redo.set_visible(false);
            refresh.set_visible(false);
        })
    };

    let open_in_viewer: Rc<dyn Fn(&Path) -> bool> = {
        let viewer = viewer.clone();
        let enter_viewer = enter_viewer.clone();
        let info = info.clone();
        Rc::new(move |path: &Path| match viewer.open(path) {
            Ok(()) => {
                enter_viewer();
                true
            }
            Err(e) => {
                info.set_text(&format!("Kann {} nicht öffnen: {e}", path.display()));
                info.set_visible(true);
                false
            }
        })
    };

    let open_setlist: Rc<dyn Fn(&Path)> = {
        let cfg = cfg.clone();
        let active = active.clone();
        let show_setlist_view = show_setlist_view.clone();
        let info = info.clone();
        Rc::new(move |path: &Path| match setlist::load(path) {
            Ok(sl) => {
                let resolved = sl.resolve(&cfg.root_dir);
                *active.borrow_mut() = Some(Active {
                    setlist: sl,
                    resolved,
                    index: None,
                });
                show_setlist_view();
            }
            Err(e) => {
                info.set_text(&format!("Kann Setlist {} nicht lesen: {e}", path.display()));
                info.set_visible(true);
            }
        })
    };

    // From the viewer, "back" returns to the setlist it was opened from, or to
    // the library if it was opened directly.
    let back_from_viewer: Rc<dyn Fn()> = {
        let active = active.clone();
        let show_library = show_library.clone();
        let show_setlist_view = show_setlist_view.clone();
        Rc::new(move || {
            if active.borrow().is_some() {
                show_setlist_view();
            } else {
                show_library();
            }
        })
    };

    let populate: Rc<dyn Fn()> = {
        let cfg = cfg.clone();
        let setlists = setlists.clone();
        let scores = scores.clone();
        let rows = rows.clone();
        let list = list.clone();
        let info = info.clone();
        Rc::new(move || {
            while let Some(row) = list.first_child() {
                list.remove(&row);
            }
            let sls = setlist::scan(&cfg.setlists_dir());
            let found = library::scan(&cfg.root_dir);
            let mut model: Vec<LibRow> = Vec::new();
            for (i, s) in sls.iter().enumerate() {
                model.push(LibRow::Setlist(i));
                list.append(&item_row(&s.name, Some("view-list-symbolic")));
            }
            for (i, e) in found.iter().enumerate() {
                model.push(LibRow::Score(i));
                list.append(&item_row(&e.rel, Some("audio-x-generic-symbolic")));
            }
            let mut msg = String::new();
            if cfg_created {
                msg.push_str(&format!(
                    "Neue Config angelegt: {}\n",
                    config::config_path().display()
                ));
            }
            if !config::root_exists(&cfg) {
                msg.push_str(&format!(
                    "Startordner {} existiert nicht – bitte root_dir in der Config anpassen.",
                    cfg.root_dir.display()
                ));
            } else if found.is_empty() {
                msg.push_str(&format!(
                    "Keine PDFs unter {} gefunden.",
                    cfg.root_dir.display()
                ));
            }
            info.set_text(msg.trim_end());
            info.set_visible(!msg.trim_end().is_empty());
            *setlists.borrow_mut() = sls;
            *scores.borrow_mut() = found;
            *rows.borrow_mut() = model;
            list.invalidate_headers();
            list.invalidate_filter();
        })
    };

    // Section headers ("Setlisten" / "Noten") drawn above the first row of
    // each section. They are not rows, so they never take keyboard focus and
    // vanish automatically when a section is filtered away.
    {
        let rows = rows.clone();
        list.set_header_func(move |row, before| {
            let rows = rows.borrow();
            let cur = rows.get(row.index() as usize);
            let prev = before.and_then(|b| rows.get(b.index() as usize));
            let text = match (cur, prev) {
                (Some(LibRow::Setlist(_)), None) => Some("Setlisten"),
                (Some(LibRow::Score(_)), None | Some(LibRow::Setlist(_))) => Some("Noten"),
                _ => None,
            };
            match text {
                Some(t) => row.set_header(Some(&section_label(t))),
                None => row.set_header(None::<&gtk::Widget>),
            }
        });
    }
    {
        let rows = rows.clone();
        let setlists = setlists.clone();
        let scores = scores.clone();
        let search = search.clone();
        list.set_filter_func(move |row| {
            let q = search.text();
            let rows = rows.borrow();
            match rows.get(row.index() as usize) {
                Some(LibRow::Setlist(i)) => {
                    if q.is_empty() {
                        *i < RECENT_SETLISTS
                    } else {
                        setlists
                            .borrow()
                            .get(*i)
                            .is_some_and(|s| setlist_matches(&s.name, &q))
                    }
                }
                Some(LibRow::Score(i)) => {
                    if q.is_empty() {
                        true
                    } else {
                        scores
                            .borrow()
                            .get(*i)
                            .is_some_and(|e| library::matches(e, &q))
                    }
                }
                None => true,
            }
        });
    }
    populate();
    {
        let list = list.clone();
        search.connect_search_changed(move |_| {
            list.invalidate_filter();
            list.invalidate_headers();
        });
    }
    {
        let populate = populate.clone();
        refresh.connect_clicked(move |_| populate());
    }

    // ----- Activating library rows -----
    {
        let rows = rows.clone();
        let setlists = setlists.clone();
        let scores = scores.clone();
        let active = active.clone();
        let open_in_viewer = open_in_viewer.clone();
        let open_setlist = open_setlist.clone();
        list.connect_row_activated(move |_, row| {
            enum Act {
                Setlist(PathBuf),
                Score(PathBuf),
            }
            let act = match rows.borrow().get(row.index() as usize) {
                Some(LibRow::Setlist(i)) => {
                    setlists.borrow().get(*i).map(|s| Act::Setlist(s.path.clone()))
                }
                Some(LibRow::Score(i)) => {
                    scores.borrow().get(*i).map(|e| Act::Score(e.path.clone()))
                }
                _ => None,
            };
            match act {
                Some(Act::Setlist(p)) => open_setlist(&p),
                Some(Act::Score(p)) => {
                    active.borrow_mut().take(); // opened directly: no setlist context
                    open_in_viewer(&p);
                }
                None => {}
            }
        });
    }

    // ----- Activating setlist entries -----
    {
        let sl_rows = sl_rows.clone();
        let active = active.clone();
        let viewer = viewer.clone();
        let enter_viewer = enter_viewer.clone();
        sl_list.connect_row_activated(move |_, row| {
            let ei = match sl_rows.borrow().get(row.index() as usize) {
                Some(SlRow::Entry(e)) => Some(*e),
                _ => None,
            };
            let Some(ei) = ei else { return };
            // Build a playlist from the resolved entries and start at `ei`, so
            // turning past a piece's edge crosses to the neighbour. `active`
            // stays set, so the viewer's back button returns here.
            let entries = {
                let a = active.borrow();
                let Some(a) = a.as_ref() else { return };
                if !a.resolved.get(ei).is_some_and(|r| r.exists) {
                    return; // missing entries stay inert until Stufe 5
                }
                a.resolved
                    .iter()
                    .map(|r| viewer::PlaylistEntry {
                        path: r.abs.clone(),
                        range: to_viewer_range(r.range),
                        broken: !r.exists
                            || matches!(r.range, Some(setlist::PageRange::Invalid)),
                        label: r.rel.clone(),
                    })
                    .collect::<Vec<_>>()
            };
            if viewer.open_playlist(entries, ei).is_ok() {
                enter_viewer();
            }
        });
    }
    // Keep the setlist view's highlighted piece in step with the viewer as
    // turning crosses pieces.
    {
        let active = active.clone();
        viewer.set_on_piece_change(move |i| {
            if let Some(a) = active.borrow_mut().as_mut() {
                a.index = Some(i);
            }
        });
    }
    {
        let show_library = show_library.clone();
        sl_back.connect_clicked(move |_| show_library());
    }

    {
        let stack = stack.clone();
        let back_from_viewer = back_from_viewer.clone();
        let show_library = show_library.clone();
        back.connect_clicked(move |_| {
            if stack.visible_child_name().as_deref() == Some("setlist") {
                show_library();
            } else {
                back_from_viewer();
            }
        });
    }
    {
        let viewer = viewer.clone();
        undo.connect_clicked(move |_| viewer.undo());
    }
    {
        let viewer = viewer.clone();
        redo.connect_clicked(move |_| viewer.redo());
    }

    // ----- Touch actions in the navigation overlay (middle tap) -----
    // Mirrors of the header actions, reachable without a keyboard even
    // in fullscreen, where the header bar is hidden.
    {
        let actions = viewer.nav_actions();
        let back2 = gtk::Button::from_icon_name("go-previous-symbolic");
        back2.set_tooltip_text(Some("Zur Bibliothek"));
        let pen2 = gtk::ToggleButton::new();
        pen2.set_icon_name("document-edit-symbolic");
        pen2.set_tooltip_text(Some("Anmerkungsmodus"));
        pen.bind_property("active", &pen2, "active")
            .bidirectional()
            .sync_create()
            .build();
        let erase2 = gtk::ToggleButton::new();
        erase2.set_icon_name("error-correct-symbolic");
        erase2.set_tooltip_text(Some("Radiergummi"));
        erase
            .bind_property("active", &erase2, "active")
            .bidirectional()
            .sync_create()
            .build();
        let undo2 = gtk::Button::from_icon_name("edit-undo-symbolic");
        undo2.set_tooltip_text(Some("Strich zurücknehmen"));
        viewer.register_undo_button(&undo2);
        let redo2 = gtk::Button::from_icon_name("edit-redo-symbolic");
        redo2.set_tooltip_text(Some("Strich wiederherstellen"));
        viewer.register_redo_button(&redo2);
        let tuner2 = gtk::ToggleButton::new();
        tuner2.set_icon_name("audio-input-microphone-symbolic");
        tuner2.set_tooltip_text(Some("Stimmgerät"));
        tuner_btn
            .bind_property("active", &tuner2, "active")
            .bidirectional()
            .sync_create()
            .build();
        let fullscreen = gtk::Button::from_icon_name("view-fullscreen-symbolic");
        fullscreen.set_tooltip_text(Some("Vollbild an/aus"));
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        actions.append(&back2);
        actions.append(&pen2);
        actions.append(&erase2);
        actions.append(&undo2);
        actions.append(&redo2);
        actions.append(&tuner2);
        actions.append(&spacer);
        actions.append(&fullscreen);

        {
            let back_from_viewer = back_from_viewer.clone();
            back2.connect_clicked(move |_| back_from_viewer());
        }
        {
            let viewer = viewer.clone();
            undo2.connect_clicked(move |_| viewer.undo());
        }
        {
            let viewer = viewer.clone();
            redo2.connect_clicked(move |_| viewer.redo());
        }
        {
            let window = window.clone();
            fullscreen.connect_clicked(move |_| {
                if window.is_fullscreen() {
                    window.unfullscreen();
                } else {
                    window.fullscreen();
                }
            });
        }
    }

    // ----- Keyboard (including foot pedal: Page Up/Down) -----
    {
        let viewer = viewer.clone();
        let stack = stack.clone();
        let window2 = window.clone();
        let show_library = show_library.clone();
        let back_from_viewer = back_from_viewer.clone();
        let pen = pen.clone();
        let erase = erase.clone();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _code, modifier| {
            use gtk::gdk::Key;
            if key == Key::F11 {
                if window2.is_fullscreen() {
                    window2.unfullscreen();
                } else {
                    window2.fullscreen();
                }
                return glib::Propagation::Stop;
            }
            if key == Key::t {
                tuner_btn.set_active(!tuner_btn.is_active());
                return glib::Propagation::Stop;
            }
            // In the setlist view, Escape steps back to the library.
            if stack.visible_child_name().as_deref() == Some("setlist") {
                if key == Key::Escape {
                    show_library();
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }
            let in_viewer = stack.visible_child_name().as_deref() == Some("viewer");
            if !in_viewer {
                return glib::Propagation::Proceed;
            }
            match key {
                Key::Page_Down | Key::Right | Key::Down | Key::space => {
                    viewer.forward();
                    glib::Propagation::Stop
                }
                Key::Page_Up | Key::Left | Key::Up | Key::BackSpace => {
                    viewer.backward();
                    glib::Propagation::Stop
                }
                Key::Escape => {
                    if viewer.nav_visible() {
                        viewer.hide_nav();
                    } else if window2.is_fullscreen() {
                        window2.unfullscreen();
                    } else {
                        back_from_viewer();
                    }
                    glib::Propagation::Stop
                }
                Key::a => {
                    pen.set_active(!pen.is_active());
                    glib::Propagation::Stop
                }
                Key::e => {
                    erase.set_active(!erase.is_active());
                    glib::Propagation::Stop
                }
                Key::z if modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK) => {
                    if modifier.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                        viewer.redo();
                    } else {
                        viewer.undo();
                    }
                    glib::Propagation::Stop
                }
                Key::y if modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK) => {
                    viewer.redo();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        window.add_controller(keys);
    }

    // Save pending annotations into the file on close.
    {
        let viewer = viewer.clone();
        window.connect_close_request(move |_| {
            viewer.flush();
            glib::Propagation::Proceed
        });
    }

    if cfg.start_fullscreen {
        window.fullscreen();
    }
    window.present();
    search.grab_focus();
}
