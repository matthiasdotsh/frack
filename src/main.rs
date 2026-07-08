// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

use frack::{config, library, tuner, viewer};

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::RefCell;
use std::rc::Rc;

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id("app.frack.Frack")
        .build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &gtk::Application) {
    let (cfg, cfg_created) = config::load_or_create();
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
    let undo = gtk::Button::from_icon_name("edit-undo-symbolic");
    undo.set_tooltip_text(Some("Strich zurücknehmen (Strg+Z)"));
    undo.set_visible(false);
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
    header.pack_end(&undo);
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
    let viewer = viewer::Viewer::new(cfg.clone(), status.clone(), pen.clone());

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

    // ----- Populate and filter the library -----
    let entries: Rc<RefCell<Vec<library::Entry>>> = Rc::new(RefCell::new(Vec::new()));

    let populate: Rc<dyn Fn()> = {
        let cfg = cfg.clone();
        let entries = entries.clone();
        let list = list.clone();
        let info = info.clone();
        Rc::new(move || {
            while let Some(row) = list.first_child() {
                list.remove(&row);
            }
            let found = library::scan(&cfg.root_dir);
            for e in &found {
                let label = gtk::Label::new(Some(&e.rel));
                label.set_xalign(0.0);
                label.set_margin_top(10);
                label.set_margin_bottom(10);
                label.set_margin_start(8);
                label.set_margin_end(8);
                list.append(&label);
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
            *entries.borrow_mut() = found;
        })
    };
    populate();

    {
        let entries = entries.clone();
        let search2 = search.clone();
        list.set_filter_func(move |row| {
            let query = search2.text();
            if query.is_empty() {
                return true;
            }
            entries
                .borrow()
                .get(row.index() as usize)
                .map(|e| library::matches(e, &query))
                .unwrap_or(true)
        });
    }
    {
        let list = list.clone();
        search.connect_search_changed(move |_| list.invalidate_filter());
    }
    {
        let populate = populate.clone();
        refresh.connect_clicked(move |_| populate());
    }

    // ----- Switching between library and viewer -----
    let show_library: Rc<dyn Fn()> = {
        let viewer = viewer.clone();
        let stack = stack.clone();
        let back = back.clone();
        let refresh = refresh.clone();
        let pen = pen.clone();
        let undo = undo.clone();
        let status = status.clone();
        let search = search.clone();
        Rc::new(move || {
            viewer.close();
            stack.set_visible_child_name("library");
            back.set_visible(false);
            pen.set_visible(false);
            undo.set_visible(false);
            refresh.set_visible(true);
            status.set_text("Bibliothek");
            search.grab_focus();
        })
    };

    {
        let entries = entries.clone();
        let viewer = viewer.clone();
        let stack = stack.clone();
        let back = back.clone();
        let refresh = refresh.clone();
        let pen = pen.clone();
        let undo = undo.clone();
        let info = info.clone();
        list.connect_row_activated(move |_, row| {
            let path = match entries.borrow().get(row.index() as usize) {
                Some(e) => e.path.clone(),
                None => return,
            };
            match viewer.open(&path) {
                Ok(()) => {
                    stack.set_visible_child_name("viewer");
                    back.set_visible(true);
                    pen.set_visible(true);
                    undo.set_visible(true);
                    refresh.set_visible(false);
                }
                Err(e) => {
                    info.set_text(&format!("Kann {} nicht öffnen: {e}", path.display()));
                    info.set_visible(true);
                }
            }
        });
    }

    {
        let show_library = show_library.clone();
        back.connect_clicked(move |_| show_library());
    }
    {
        let viewer = viewer.clone();
        undo.connect_clicked(move |_| viewer.undo());
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
        let undo2 = gtk::Button::from_icon_name("edit-undo-symbolic");
        undo2.set_tooltip_text(Some("Strich zurücknehmen"));
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
        actions.append(&undo2);
        actions.append(&tuner2);
        actions.append(&spacer);
        actions.append(&fullscreen);

        {
            let show_library = show_library.clone();
            back2.connect_clicked(move |_| show_library());
        }
        {
            let viewer = viewer.clone();
            undo2.connect_clicked(move |_| viewer.undo());
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
        let pen = pen.clone();
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
                        show_library();
                    }
                    glib::Propagation::Stop
                }
                Key::a => {
                    pen.set_active(!pen.is_active());
                    glib::Propagation::Stop
                }
                Key::z if modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK) => {
                    viewer.undo();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        window.add_controller(keys);
    }

    // Burn pending annotations into the file on close.
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
