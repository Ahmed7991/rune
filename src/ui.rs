//! The app shell: a project sidebar on the left, a tab strip of `claude`
//! terminals on the right. Owns persistence (open tabs/projects restore +
//! resume across restarts).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, Application, ApplicationWindow, Button, DropDown, Entry, EventControllerKey,
    FileDialog, FlowBox, GestureClick, HeaderBar, Image, Label, ListBox, ListBoxRow, Notebook,
    Orientation, Paned, PolicyType, Popover, PopoverMenu, PositionType, ScrolledWindow,
    SelectionMode, Stack, Switch, Window,
};
use vte4::{Terminal, TerminalExt};

use crate::claude;
use crate::config::{self, Config, OpenTab};
use crate::cost;
use crate::git;
use crate::hooks;
use crate::palette;
use crate::queue::{self, Need};
use crate::sessions::{self, SessionMeta};
use crate::status::{self, Status};
use crate::terminal;

/// In-memory state for one open tab. The `terminal` handle is kept so we can
/// find a tab's page (which may have been reordered) and persist tabs in
/// display order.
struct TabInfo {
    project_path: String,
    session_id: String,
    title: String,
    terminal: Terminal,
    /// Status dot in the tab label, updated from `~/.claude/sessions/*.json`.
    dot: Label,
    /// Previous live status, to detect the busy→idle "finished" transition.
    last_status: Option<Status>,
}

/// Shared mutable app state. Held behind `Rc<RefCell<…>>`; closures clone the
/// `Rc` and borrow briefly when they fire (never across a re-entrant call).
struct AppState {
    config: Config,
    tabs: Vec<TabInfo>,
    selected_project: Option<String>,
    notebook: Notebook,
    /// Right-pane stack: "home" (the dashboard) ↔ "sessions" (the terminals).
    stack: Stack,
    /// The dashboard's scrollable content box — cleared + rebuilt on show.
    dash_box: gtk4::Box,
    sidebar: ListBox,
    new_session_btn: Button,
    resume_btn: Button,
    /// The "needs-you" queue button + its count badge, refreshed on the status
    /// poll so the header always shows how many sessions are waiting on you.
    queue_btn: Button,
    queue_badge: Label,
    /// Last (waiting, working) counts the dashboard was rebuilt for, so the poll
    /// can live-refresh the home view only when those actually change.
    last_dash_counts: (usize, usize),
    /// Hash of the hook-reported phase/hint of the live sessions at the last
    /// dashboard rebuild. The hooks can change a row (working→blocked, a new
    /// hint) without moving the waiting/working counts, so the poll also
    /// refreshes when this fingerprint changes. `0` when hooks are off.
    last_dash_hook_fp: u64,
    /// Per-project estimated total cost, cached by a fingerprint of the
    /// project's transcript dir so the (heavy) parse only reruns when a
    /// transcript actually changes. project path → (dir fingerprint, cost).
    cost_cache: Rc<RefCell<HashMap<String, (u64, f64)>>>,
    /// The cost Label on each project card of the *current* dashboard build,
    /// keyed by project — so a background cost computation can fill it in when
    /// it finishes. Cleared + repopulated every dashboard rebuild.
    dash_cost_labels: Rc<RefCell<HashMap<String, Label>>>,
    /// Projects whose per-project cost is currently being computed on a worker
    /// thread, so a rebuild doesn't spawn a duplicate worker re-parsing the same
    /// transcripts. Main-thread only; the drain removes a project when its
    /// result lands (or on worker exit).
    cost_in_flight: Rc<RefCell<HashSet<String>>>,
    /// The glanceable status badge Label on each rail row, keyed by project,
    /// paired with the status it currently shows so the poll only repaints on a
    /// transition (like the per-tab dot path) instead of every tick. Cleared +
    /// repopulated every `rebuild_sidebar`.
    rail_badges: Rc<RefCell<HashMap<String, (Label, RailStatus)>>>,
}

type State = Rc<RefCell<AppState>>;

/// The Mission Control finish is a dark design language and the home dashboard
/// is unconditionally navy, so the app ships dark by default; an explicit
/// `prefer_dark: false` in settings still lets a user force light. Kept in one
/// place so `build_ui` and the settings toggle agree on the default.
const DEFAULT_PREFER_DARK: bool = true;

pub fn build_ui(app: &Application) {
    let config = Config::load();
    install_css();
    apply_prefer_dark(Some(config.settings.prefer_dark.unwrap_or(DEFAULT_PREFER_DARK)));

    // ── header bar (Mission Control chrome) ────────────────────────────────
    let header = HeaderBar::new();
    header.add_css_class("rune-header");

    // Brand: the app mark + a "rune_" wordmark over a "MISSION CONTROL" sublabel.
    let brand = gtk4::Box::new(Orientation::Horizontal, 9);
    brand.add_css_class("brand");
    let mark = Image::from_icon_name(crate::APP_ID);
    mark.set_pixel_size(24);
    mark.add_css_class("brand-mark");
    let wordbox = gtk4::Box::new(Orientation::Vertical, 0);
    wordbox.set_valign(gtk4::Align::Center);
    let wordmark = Label::new(None);
    wordmark.set_markup("rune<span foreground=\"#36d3fa\">_</span>");
    wordmark.set_xalign(0.0);
    wordmark.add_css_class("brand-word");
    let brand_sub = Label::new(Some("MISSION CONTROL"));
    brand_sub.set_xalign(0.0);
    brand_sub.add_css_class("brand-sub");
    wordbox.append(&wordmark);
    wordbox.append(&brand_sub);
    brand.append(&mark);
    brand.append(&wordbox);
    header.pack_start(&brand);

    let home_btn = Button::from_icon_name("go-home-symbolic");
    home_btn.add_css_class("flat");
    home_btn.set_tooltip_text(Some("Home — the cross-project overview"));
    header.pack_start(&home_btn);

    let new_session_btn = Button::from_icon_name("tab-new-symbolic");
    new_session_btn.add_css_class("flat");
    new_session_btn.set_tooltip_text(Some("New Claude session in the selected project"));
    new_session_btn.set_sensitive(false);
    header.pack_start(&new_session_btn);

    let resume_btn = Button::from_icon_name("document-open-recent-symbolic");
    resume_btn.add_css_class("flat");
    resume_btn.set_tooltip_text(Some("Browse & resume past sessions for the selected project"));
    resume_btn.set_sensitive(false);
    header.pack_start(&resume_btn);

    // The cockpit headline: a cross-project "needs-you" queue. Icon + a count
    // badge that shows how many sessions are waiting on you (refreshed on poll).
    let queue_btn = Button::new();
    queue_btn.add_css_class("flat");
    queue_btn.set_tooltip_text(Some("Needs-you queue — live sessions across all projects"));
    let queue_box = gtk4::Box::new(Orientation::Horizontal, 4);
    queue_box.append(&Image::from_icon_name("view-list-symbolic"));
    let queue_badge = Label::new(None);
    queue_badge.add_css_class("queue-badge");
    queue_badge.set_visible(false);
    queue_box.append(&queue_badge);
    queue_btn.set_child(Some(&queue_box));
    header.pack_start(&queue_btn);

    // Always-visible search field (centered title) — a click opens the Ctrl-K
    // command palette. It's a button dressed as a search box, not a live entry:
    // the palette is the real search surface, this is its discoverable doorway.
    let search = Button::new();
    search.add_css_class("rune-search");
    search.set_tooltip_text(Some("Search sessions, projects & prompts (Ctrl+K)"));
    let search_box = gtk4::Box::new(Orientation::Horizontal, 9);
    let search_icon = Image::from_icon_name("system-search-symbolic");
    search_icon.add_css_class("search-icon");
    let search_ph = Label::new(Some("Search sessions, projects, prompts…"));
    search_ph.set_xalign(0.0);
    search_ph.set_hexpand(true);
    search_ph.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    search_ph.add_css_class("search-ph");
    let search_kbd = Label::new(Some("Ctrl K"));
    search_kbd.add_css_class("kbd");
    search_box.append(&search_icon);
    search_box.append(&search_ph);
    search_box.append(&search_kbd);
    search.set_child(Some(&search_box));
    search.set_size_request(360, -1);
    header.set_title_widget(Some(&search));

    let settings_btn = Button::from_icon_name("preferences-system-symbolic");
    settings_btn.add_css_class("flat");
    settings_btn.set_tooltip_text(Some("Settings"));
    header.pack_end(&settings_btn);

    // ── project rail ──────────────────────────────────────────────────────
    let sidebar = ListBox::new();
    sidebar.set_selection_mode(SelectionMode::Single);
    sidebar.add_css_class("navigation-sidebar");
    sidebar.add_css_class("rail-list");

    let sidebar_scroll = ScrolledWindow::builder()
        .child(&sidebar)
        .vexpand(true)
        .hscrollbar_policy(PolicyType::Never)
        .build();

    // Rail head: a cyan tick + the "PROJECTS" label + an inline "add folder".
    let rail_head = gtk4::Box::new(Orientation::Horizontal, 8);
    rail_head.add_css_class("rail-head");
    let rail_tick = gtk4::Box::new(Orientation::Horizontal, 0);
    rail_tick.add_css_class("rail-tick");
    rail_tick.set_size_request(12, 2);
    rail_tick.set_valign(gtk4::Align::Center);
    let rail_head_label = Label::new(Some("PROJECTS"));
    rail_head_label.set_xalign(0.0);
    rail_head_label.set_hexpand(true);
    rail_head_label.add_css_class("rail-head-label");
    let add_btn = Button::from_icon_name("list-add-symbolic");
    add_btn.add_css_class("flat");
    add_btn.add_css_class("rail-add");
    add_btn.set_tooltip_text(Some("Add a project folder"));
    rail_head.append(&rail_tick);
    rail_head.append(&rail_head_label);
    rail_head.append(&add_btn);

    // Rail foot: the live "watching ~/.claude" indicator.
    let rail_foot = gtk4::Box::new(Orientation::Horizontal, 8);
    rail_foot.add_css_class("rail-foot");
    let rail_live = gtk4::Box::new(Orientation::Horizontal, 0);
    rail_live.add_css_class("rail-live");
    rail_live.set_size_request(6, 6);
    rail_live.set_valign(gtk4::Align::Center);
    let rail_foot_label = Label::new(Some("Watching ~/.claude · live"));
    rail_foot_label.set_xalign(0.0);
    rail_foot_label.add_css_class("rail-foot-label");
    rail_foot.append(&rail_live);
    rail_foot.append(&rail_foot_label);

    let sidebar_box = gtk4::Box::new(Orientation::Vertical, 0);
    sidebar_box.add_css_class("rune-rail");
    sidebar_box.append(&rail_head);
    sidebar_box.append(&sidebar_scroll);
    sidebar_box.append(&rail_foot);
    sidebar_box.set_size_request(232, -1);

    // ── content: a stack of [ home dashboard | session terminals ] ────────
    let notebook = Notebook::new();
    notebook.set_scrollable(true);
    notebook.set_hexpand(true);
    notebook.set_vexpand(true);

    let dash_box = gtk4::Box::new(Orientation::Vertical, 16);
    dash_box.add_css_class("rune-dash");
    dash_box.set_hexpand(true);
    dash_box.set_vexpand(true);
    let dash_scroll = ScrolledWindow::builder()
        .child(&dash_box)
        .hscrollbar_policy(PolicyType::Never)
        .hexpand(true)
        .vexpand(true)
        .build();

    let stack = Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.add_named(&dash_scroll, Some("home"));
    stack.add_named(&notebook, Some("sessions"));

    let paned = Paned::new(Orientation::Horizontal);
    paned.set_start_child(Some(&sidebar_box));
    paned.set_end_child(Some(&stack));
    paned.set_position(232);
    paned.set_resize_start_child(false);
    paned.set_shrink_start_child(false);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("rune")
        .default_width(1100)
        .default_height(720)
        .build();
    window.set_titlebar(Some(&header));
    window.set_child(Some(&paned));

    // ── shared state ──────────────────────────────────────────────────────
    let state: State = Rc::new(RefCell::new(AppState {
        config,
        tabs: Vec::new(),
        selected_project: None,
        notebook: notebook.clone(),
        stack: stack.clone(),
        dash_box: dash_box.clone(),
        sidebar: sidebar.clone(),
        new_session_btn: new_session_btn.clone(),
        resume_btn: resume_btn.clone(),
        queue_btn: queue_btn.clone(),
        queue_badge: queue_badge.clone(),
        last_dash_counts: (usize::MAX, usize::MAX),
        last_dash_hook_fp: 0,
        cost_cache: Rc::new(RefCell::new(HashMap::new())),
        dash_cost_labels: Rc::new(RefCell::new(HashMap::new())),
        cost_in_flight: Rc::new(RefCell::new(HashSet::new())),
        rail_badges: Rc::new(RefCell::new(HashMap::new())),
    }));
    {
        // Restore the last selection, but drop it if it no longer names a known
        // project (stale config, hand-edit, removed folder) — otherwise the
        // "New session" button would be live with nothing highlighted.
        let mut s = state.borrow_mut();
        let restored = s.config.selected_project.clone();
        s.selected_project = restored.filter(|p| s.config.projects.contains(p));
    }

    // ── signals ───────────────────────────────────────────────────────────
    {
        let st = state.clone();
        sidebar.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let idx = row.index();
                let mut s = st.borrow_mut();
                if let Some(path) = s.config.projects.get(idx as usize).cloned() {
                    s.selected_project = Some(path);
                    s.new_session_btn.set_sensitive(true);
                    s.resume_btn.set_sensitive(true);
                }
            }
        });
    }
    {
        let st = state.clone();
        let win = window.clone();
        add_btn.connect_clicked(move |_| {
            let dialog = FileDialog::builder().title("Add a project folder").build();
            let st = st.clone();
            dialog.select_folder(Some(&win), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        let path = config::normalize_project_path(&path.to_string_lossy());
                        let added = {
                            let mut s = st.borrow_mut();
                            if s.config.projects.contains(&path) {
                                false
                            } else {
                                s.config.projects.push(path);
                                true
                            }
                        };
                        if added {
                            rebuild_sidebar(&st);
                            save_state(&st);
                        }
                    }
                }
            });
        });
    }
    {
        let st = state.clone();
        new_session_btn.connect_clicked(move |_| {
            let proj = st.borrow().selected_project.clone();
            if let Some(path) = proj {
                add_tab(&st, &path, claude::new_session_id(), None, true);
                save_state(&st);
            }
        });
    }
    {
        let st = state.clone();
        let btn = resume_btn.clone();
        resume_btn.connect_clicked(move |_| open_resume_popover(&st, &btn));
    }
    {
        let st = state.clone();
        let btn = queue_btn.clone();
        queue_btn.connect_clicked(move |_| open_queue_popover(&st, &btn));
    }
    {
        let st = state.clone();
        let win = window.clone();
        settings_btn.connect_clicked(move |_| open_settings(&st, &win));
    }
    {
        let st = state.clone();
        home_btn.connect_clicked(move |_| toggle_home(&st));
    }
    {
        // The header search field is the discoverable doorway to the Ctrl-K
        // command palette (the real search surface).
        let st = state.clone();
        let win = window.clone();
        search.connect_clicked(move |_| open_quick_switcher(&st, &win));
    }
    {
        // Focus the terminal of whichever tab the user switches to.
        notebook.connect_switch_page(move |_, child, _| {
            if let Ok(term) = child.clone().downcast::<Terminal>() {
                term.grab_focus();
            }
        });
    }
    {
        let st = state.clone();
        window.connect_close_request(move |_| {
            save_state(&st);
            glib::Propagation::Proceed
        });
    }
    // Also persist on SIGTERM/SIGINT (logout, `kill`, Ctrl-C from a launching
    // terminal) — a daily-driver shouldn't lose its open tabs to a hard exit.
    for signum in [2 /* SIGINT */, 15 /* SIGTERM */] {
        let st = state.clone();
        let app = app.clone();
        glib::unix_signal_add_local(signum, move || {
            save_state(&st);
            app.quit();
            glib::ControlFlow::Break
        });
    }

    // Global Ctrl-K quick-switcher. Capture phase so it fires before the focused
    // VTE terminal consumes the key. (Ctrl-K is readline kill-line in a shell;
    // intercepting it app-wide is a deliberate cockpit trade-off for fast nav.)
    {
        let st = state.clone();
        let win = window.clone();
        let key = EventControllerKey::new();
        key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, keyval, _code, mods| {
            let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
            let other = mods.contains(gdk::ModifierType::ALT_MASK)
                || mods.contains(gdk::ModifierType::SUPER_MASK);
            if ctrl && !other && matches!(keyval, gdk::Key::k | gdk::Key::K) {
                open_quick_switcher(&st, &win);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(key);
    }

    // ── populate + restore ────────────────────────────────────────────────
    rebuild_sidebar(&state);

    let saved_tabs = state.borrow().config.open_tabs.clone();
    for tab in saved_tabs {
        // Defense-in-depth: never spawn a session whose id isn't a real UUID
        // (a hand-edited/corrupt config shouldn't reach the CLI or the shell).
        if !claude::is_valid_session_id(&tab.session_id) {
            eprintln!("rune: skipping restored tab with invalid session id {:?}", tab.session_id);
            continue;
        }
        add_tab(&state, &tab.project_path, tab.session_id, Some(tab.title), false);
    }
    if state.borrow().notebook.n_pages() > 0 {
        state.borrow().notebook.set_current_page(Some(0));
    }

    // Land on the home overview — the cockpit's front door. Restored session
    // terminals live on the "sessions" stack page, one Home-toggle away.
    show_home(&state);

    // Live per-tab status from ~/.claude/sessions/*.json — polled, no hooks and
    // no changes to the user's Claude config.
    {
        let st = state.clone();
        let app = app.clone();
        refresh_statuses(&st, &app);
        glib::timeout_add_local(std::time::Duration::from_millis(1500), move || {
            refresh_statuses(&st, &app);
            glib::ControlFlow::Continue
        });
    }

    window.present();
    // We land on the home overview, so there's no terminal to focus here; the
    // session paths (add_tab / resume_session) grab focus when you open one.
}

// ───────────────────────────────────────────────────────────────────────────
// Sidebar
// ───────────────────────────────────────────────────────────────────────────

fn rebuild_sidebar(state: &State) {
    let (sidebar, projects, selected, rail_badges) = {
        let s = state.borrow();
        (
            s.sidebar.clone(),
            s.config.projects.clone(),
            s.selected_project.clone(),
            s.rail_badges.clone(),
        )
    };

    while let Some(child) = sidebar.first_child() {
        sidebar.remove(&child);
    }
    // The rows (and their badge labels) are about to be recreated — drop the
    // stale handles so the poll only ever paints badges that are on screen.
    rail_badges.borrow_mut().clear();

    let mut row_to_select: Option<ListBoxRow> = None;
    for path in &projects {
        let row = make_project_row(state, path);
        sidebar.append(&row);
        if selected.as_deref() == Some(path.as_str()) {
            row_to_select = Some(row);
        }
    }
    if let Some(row) = row_to_select {
        sidebar.select_row(Some(&row));
    }

    // Paint the badges once now from a fresh scan, so a just-rebuilt rail is
    // accurate immediately; the 1.5s status poll keeps them live thereafter.
    let needs: Vec<(String, Need)> = status::live_sessions()
        .iter()
        .map(|ls| (ls.cwd.clone(), queue::live_need(ls)))
        .collect();
    update_rail_badges(state, &needs);

    let s = state.borrow();
    s.new_session_btn.set_sensitive(selected.is_some());
    s.resume_btn.set_sensitive(selected.is_some());
}

fn make_project_row(state: &State, path: &str) -> ListBoxRow {
    let row = ListBoxRow::new();
    row.add_css_class("proj-row");
    let hbox = gtk4::Box::new(Orientation::Horizontal, 9);
    hbox.set_margin_start(9);
    hbox.set_margin_end(9);
    hbox.set_margin_top(6);
    hbox.set_margin_bottom(6);

    // The project's identity swatch — the same colour used for it everywhere.
    hbox.append(&make_swatch(path, 8));

    let label = Label::builder()
        .label(basename(path))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        // Full path (names are middle-ellipsized) + a hint that reorder/remove
        // live on the right-click menu, since this row no longer has an inline
        // remove button.
        .tooltip_text(format!("{path}\nRight-click to reorder or remove"))
        .build();
    label.add_css_class("proj-name");
    hbox.append(&label);

    // Glanceable per-project status badge (your-turn / working / idle). Starts
    // as idle ("·") and is repainted in place on the poll; kept in `rail_badges`
    // by project, paired with the status it shows so unchanged ticks are skipped.
    let badge = Label::new(Some("·"));
    badge.add_css_class("rail-badge");
    badge.add_css_class("badge-idle");
    badge.set_valign(gtk4::Align::Center);
    state
        .borrow()
        .rail_badges
        .borrow_mut()
        .insert(path.to_string(), (badge.clone(), RailStatus::Idle));
    hbox.append(&badge);

    row.set_child(Some(&hbox));

    attach_reorder_menu(state, &row, path);
    row
}

/// Right-click a project row → Move up / Move down / Remove (manage the rail).
fn attach_reorder_menu(state: &State, row: &ListBoxRow, path: &str) {
    let menu = gio::Menu::new();
    menu.append(Some("Launch settings…"), Some("project.preset"));
    let manage = gio::Menu::new();
    manage.append(Some("Move up"), Some("project.up"));
    manage.append(Some("Move down"), Some("project.down"));
    manage.append(Some("Remove from rail"), Some("project.remove"));
    menu.append_section(None, &manage);

    let actions = gio::SimpleActionGroup::new();
    let preset = gio::SimpleAction::new("preset", None);
    {
        let st = state.clone();
        let path = path.to_string();
        let row_weak = row.downgrade();
        preset.connect_activate(move |_, _| {
            let win = row_weak
                .upgrade()
                .and_then(|r| r.root())
                .and_downcast::<gtk4::Window>();
            open_project_preset(&st, &path, win.as_ref());
        });
    }
    let up = gio::SimpleAction::new("up", None);
    {
        let st = state.clone();
        let path = path.to_string();
        up.connect_activate(move |_, _| move_project(&st, &path, -1));
    }
    let down = gio::SimpleAction::new("down", None);
    {
        let st = state.clone();
        let path = path.to_string();
        down.connect_activate(move |_, _| move_project(&st, &path, 1));
    }
    let remove = gio::SimpleAction::new("remove", None);
    {
        let st = state.clone();
        let path = path.to_string();
        remove.connect_activate(move |_, _| remove_project(&st, &path));
    }
    actions.add_action(&preset);
    actions.add_action(&up);
    actions.add_action(&down);
    actions.add_action(&remove);
    row.insert_action_group("project", Some(&actions));

    let popover = PopoverMenu::from_model(Some(&menu));
    popover.set_parent(row);
    popover.set_has_arrow(false);
    popover.set_halign(gtk4::Align::Start);

    let gesture = GestureClick::new();
    gesture.set_button(gdk::BUTTON_SECONDARY);
    gesture.connect_pressed(glib::clone!(
        #[weak]
        popover,
        move |_, _, x, y| {
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    ));
    row.add_controller(gesture);

    row.connect_destroy(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));
}

fn move_project(state: &State, path: &str, delta: i32) {
    {
        let mut s = state.borrow_mut();
        if let Some(i) = s.config.projects.iter().position(|p| p == path) {
            let j = i as i32 + delta;
            if j >= 0 && (j as usize) < s.config.projects.len() {
                s.config.projects.swap(i, j as usize);
            }
        }
    }
    rebuild_sidebar(state);
    save_state(state);
}

/// Drop a project from the rail (and clear it as the selection if it was). Used
/// by the row context menu's "Remove from rail".
fn remove_project(state: &State, path: &str) {
    {
        let mut s = state.borrow_mut();
        s.config.projects.retain(|p| p != path);
        // Drop the project's launch preset too, so it doesn't linger as dead
        // config and silently resurrect (e.g. a forgotten bypassPermissions) if
        // the same path is re-added later.
        s.config
            .project_presets
            .remove(&config::normalize_project_path(path));
        if s.selected_project.as_deref() == Some(path) {
            s.selected_project = None;
        }
    }
    rebuild_sidebar(state);
    save_state(state);
}

/// A small square painted in a project's identity colour. `size` is its side in
/// px. The colour comes from a CSS class (`swatch-<n>`), defined from the same
/// palette in [`install_css`].
fn make_swatch(project: &str, size: i32) -> gtk4::Box {
    let sw = gtk4::Box::new(Orientation::Horizontal, 0);
    sw.add_css_class("swatch");
    sw.add_css_class(&format!("swatch-{}", palette::color_index(project)));
    sw.set_size_request(size, size);
    sw.set_halign(gtk4::Align::Center);
    sw.set_valign(gtk4::Align::Center);
    sw
}

/// The glanceable per-project state shown as a rail badge. Blocked (paused on a
/// permission) outranks "needs you" (idle / waiting), which outranks "working",
/// which outranks idle — the badge always surfaces the most actionable state for
/// that project, and uses the same colour language as the command board so one
/// colour means one thing everywhere.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RailStatus {
    Blocked(usize),
    YourTurn(usize),
    Working(usize),
    Idle,
}

/// Tally a project's live sessions (matched by exact cwd) into a glanceable
/// status, from the reconciled per-session needs (so a permission block shows on
/// the rail too). Sessions started in a subdirectory still appear in the
/// dashboard queue under their own cwd; they just don't light up a rail row that
/// doesn't exactly own them.
fn rail_status_for(project: &str, needs: &[(String, Need)]) -> RailStatus {
    let proj = config::normalize_project_path(project);
    let mut blocked = 0usize;
    let mut waiting = 0usize;
    let mut working = 0usize;
    for (cwd, need) in needs {
        if config::normalize_project_path(cwd) != proj {
            continue;
        }
        match need {
            Need::Blocked => blocked += 1,
            Need::YourTurn | Need::JustFinished => waiting += 1,
            Need::Working => working += 1,
        }
    }
    if blocked > 0 {
        RailStatus::Blocked(blocked)
    } else if waiting > 0 {
        RailStatus::YourTurn(waiting)
    } else if working > 0 {
        RailStatus::Working(working)
    } else {
        RailStatus::Idle
    }
}

/// Repaint one rail badge: its count + colour class + tooltip.
fn set_rail_badge(badge: &Label, status: RailStatus) {
    for c in ["badge-blocked", "badge-turn", "badge-work", "badge-idle"] {
        badge.remove_css_class(c);
    }
    let (text, class, tip) = match status {
        RailStatus::Blocked(n) => (
            n.to_string(),
            "badge-blocked",
            format!("{n} session{} paused on a permission", plural(n)),
        ),
        RailStatus::YourTurn(n) => (
            n.to_string(),
            "badge-turn",
            format!("{n} session{} waiting on you", plural(n)),
        ),
        RailStatus::Working(n) => (
            n.to_string(),
            "badge-work",
            format!("{n} session{} working", plural(n)),
        ),
        RailStatus::Idle => ("·".to_string(), "badge-idle", "Idle".to_string()),
    };
    badge.set_text(&text);
    badge.add_css_class(class);
    badge.set_tooltip_text(Some(&tip));
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Refresh the rail status badges from the latest live-session scan, repainting
/// only the ones whose status actually changed (the per-tab dot path does the
/// same — avoids churning markup/allocations every tick). Called on the 1.5s
/// poll (reusing its scan — no transcript reads, no full rebuild) and once on
/// each `rebuild_sidebar`.
fn update_rail_badges(state: &State, needs: &[(String, Need)]) {
    let badges = state.borrow().rail_badges.clone();
    for (project, (badge, shown)) in badges.borrow_mut().iter_mut() {
        let now = rail_status_for(project, needs);
        if now != *shown {
            set_rail_badge(badge, now);
            *shown = now;
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Tabs
// ───────────────────────────────────────────────────────────────────────────

fn add_tab(state: &State, project_path: &str, session_id: String, title: Option<String>, focus: bool) {
    let preset = state.borrow().config.preset_for(project_path);
    let term = terminal::spawn_session(project_path, &session_id, &preset);
    // Honor a configured terminal font for newly spawned sessions too.
    if let Some(font) = state.borrow().config.settings.terminal_font.clone() {
        apply_terminal_font(&term, &font);
    }
    // Use a persisted title if we have one (forward-compat with real session
    // titles in Increment 2); otherwise fall back to the project basename.
    let title = title
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| basename(project_path));
    let dot = make_status_dot();
    let tab_label = make_tab_label(state, &term, &title, &dot);

    {
        let s = state.borrow();
        let page = s.notebook.append_page(&term, Some(&tab_label));
        s.notebook.set_tab_reorderable(&term, true);
        if focus {
            s.notebook.set_current_page(Some(page));
            s.stack.set_visible_child_name("sessions");
        }
    }

    // When the session's shell finally exits, drop the tab. Deferred to idle so
    // we don't mutate the widget tree from inside the terminal's own signal.
    {
        let st = state.clone();
        term.connect_child_exited(move |term, _status| {
            let st = st.clone();
            let term = term.clone();
            glib::idle_add_local_once(move || close_tab(&st, &term));
        });
    }

    state.borrow_mut().tabs.push(TabInfo {
        project_path: project_path.to_string(),
        session_id,
        title,
        terminal: term.clone(),
        dot,
        last_status: None,
    });

    if focus {
        term.grab_focus();
    }
}

fn make_tab_label(state: &State, term: &Terminal, title: &str, dot: &Label) -> gtk4::Box {
    let hbox = gtk4::Box::new(Orientation::Horizontal, 6);
    hbox.append(dot);
    // Project basenames are short; show them in full. (A pathologically long
    // folder name just makes a wider tab — acceptable, and the strip scrolls.)
    let label = Label::new(Some(title));

    let close = Button::from_icon_name("window-close-symbolic");
    close.add_css_class("flat");
    close.set_tooltip_text(Some("Close this session tab"));
    {
        let st = state.clone();
        let term = term.clone();
        close.connect_clicked(move |_| close_tab(&st, &term));
    }

    hbox.append(&label);
    hbox.append(&close);
    hbox
}

fn close_tab(state: &State, term: &Terminal) {
    {
        let mut s = state.borrow_mut();
        if let Some(page) = s.notebook.page_num(term) {
            s.notebook.remove_page(Some(page));
        }
        s.tabs.retain(|t| &t.terminal != term);
    }
    save_state(state);
    // Closing the last session would otherwise strand the user on an empty
    // terminal pane — fall back to the home overview instead.
    if state.borrow().notebook.n_pages() == 0 {
        show_home(state);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Resume browser
// ───────────────────────────────────────────────────────────────────────────

/// Pop a list of the selected project's past sessions under `anchor`; clicking
/// one resumes it into a tab (or focuses it if already open).
fn open_resume_popover(state: &State, anchor: &Button) {
    let Some(project) = state.borrow().selected_project.clone() else {
        return;
    };
    let session_list = sessions::list_sessions(&project);

    let listbox = ListBox::new();
    listbox.add_css_class("boxed-list");

    let content = gtk4::Box::new(Orientation::Vertical, 8);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(10);
    content.set_margin_end(10);
    content.set_size_request(380, -1);

    let heading = Label::builder()
        .label(format!("Sessions · {}", basename(&project)))
        .xalign(0.0)
        .build();
    heading.add_css_class("heading");
    content.append(&heading);

    if !session_list.is_empty() {
        let total: f64 = session_list.iter().map(|m| m.cost_usd).sum();
        // The list is capped at MAX_SESSIONS, so on a long-lived project this is
        // the cost of the recent sessions shown — not the project's all-time sum.
        let n = session_list.len();
        let label = if n >= sessions::MAX_SESSIONS {
            format!("{n} recent sessions · ~{} shown", cost::format_usd(total))
        } else {
            format!(
                "{n} session{} · ~{} total",
                if n == 1 { "" } else { "s" },
                cost::format_usd(total)
            )
        };
        let summary = Label::builder().label(label).xalign(0.0).build();
        summary.add_css_class("dim-label");
        summary.add_css_class("caption");
        summary.set_tooltip_text(Some(
            "Estimated from public per-model token rates; excludes batch/priority pricing and may drift.",
        ));
        content.append(&summary);
    }

    let popover = Popover::new();

    if session_list.is_empty() {
        listbox.set_selection_mode(SelectionMode::None);
        let row = ListBoxRow::new();
        row.set_activatable(false);
        let empty = Label::builder()
            .label("No past sessions for this project yet.")
            .xalign(0.0)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .build();
        empty.add_css_class("dim-label");
        row.set_child(Some(&empty));
        listbox.append(&row);
    } else {
        for meta in &session_list {
            listbox.append(&make_session_row(meta));
        }
        // index → (id, title), parallel to the rows appended above.
        let ids: Vec<(String, String)> = session_list
            .iter()
            .map(|m| (m.id.clone(), m.title.clone()))
            .collect();
        let st = state.clone();
        let proj = project.clone();
        let pop = popover.clone();
        listbox.connect_row_activated(move |_, row| {
            if let Some((id, title)) = ids.get(row.index() as usize) {
                resume_session(&st, &proj, id, title);
                pop.popdown();
            }
        });
    }

    let scroller = ScrolledWindow::builder()
        .child(&listbox)
        .hscrollbar_policy(PolicyType::Never)
        .max_content_height(420)
        .propagate_natural_height(true)
        .build();
    content.append(&scroller);

    popover.set_child(Some(&content));
    popover.set_parent(anchor);
    popover.set_position(PositionType::Bottom);
    popover.connect_closed(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));
    popover.popup();
}

fn make_session_row(meta: &SessionMeta) -> ListBoxRow {
    let row = ListBoxRow::new();
    let vbox = gtk4::Box::new(Orientation::Vertical, 2);
    vbox.set_margin_top(6);
    vbox.set_margin_bottom(6);
    vbox.set_margin_start(8);
    vbox.set_margin_end(8);

    let title = Label::builder()
        .label(&meta.title)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();

    let prompts = if meta.prompt_count == 1 { "prompt" } else { "prompts" };
    let subtitle = Label::builder()
        .label(format!(
            "{} · {} {} · ~{}",
            sessions::relative_time(meta.modified),
            meta.prompt_count,
            prompts,
            cost::format_usd(meta.cost_usd),
        ))
        .xalign(0.0)
        .build();
    subtitle.add_css_class("dim-label");
    subtitle.add_css_class("caption");

    vbox.append(&title);
    vbox.append(&subtitle);
    row.set_child(Some(&vbox));
    row
}

/// Focus the session's tab if it's already open; otherwise open it as a new
/// tab that resumes the session.
fn resume_session(state: &State, project: &str, id: &str, title: &str) {
    let existing = {
        let s = state.borrow();
        s.tabs
            .iter()
            .find(|t| t.session_id == id)
            .map(|t| t.terminal.clone())
    };
    if let Some(term) = existing {
        let s = state.borrow();
        if let Some(page) = s.notebook.page_num(&term) {
            s.notebook.set_current_page(Some(page));
        }
        s.stack.set_visible_child_name("sessions");
        drop(s);
        term.grab_focus();
        return;
    }
    add_tab(state, project, id.to_string(), Some(title.to_string()), true);
    save_state(state);
}

// ───────────────────────────────────────────────────────────────────────────
// Live status (per-tab working / idle dot + notify on done)
// ───────────────────────────────────────────────────────────────────────────

fn make_status_dot() -> Label {
    let dot = Label::new(None);
    dot.set_valign(gtk4::Align::Center);
    set_status_dot(&dot, None);
    dot
}

fn set_status_dot(dot: &Label, status: Option<Status>) {
    // Mission Control status language (kept identical everywhere — tab dots,
    // queue rows, rail badges): green = working, amber = your turn (waiting on
    // you). See `need_appearance` and the `.badge-*` CSS.
    let (color, tip) = match status {
        Some(Status::Busy) => ("#43e08a", "Working…"),
        Some(Status::Idle) => ("#ffb13b", "Your turn — ready for input"),
        None => ("#5e5c64", "Not running"),
    };
    dot.set_markup(&format!("<span foreground=\"{color}\">\u{25cf}</span>"));
    dot.set_tooltip_text(Some(tip));
}

/// Poll the live session state files, repaint each tab's dot, refresh the
/// cross-project queue badge, and desktop-notify when a *background* tab
/// transitions busy→idle (Claude finished there).
fn refresh_statuses(state: &State, app: &Application) {
    // One scan drives the per-tab dots, the rail badges, and the global queue.
    let live = status::live_sessions();
    // Reconcile each live session's need the SAME way the dashboard does (hook
    // state preferred when fresher) so the always-visible header badge + rail
    // agree with the command board — the raw busy/idle heartbeat can't see
    // `Blocked`. Cheap: no transcript reads.
    let needs: Vec<(String, Need)> = live
        .iter()
        .map(|ls| (ls.cwd.clone(), queue::live_need(ls)))
        .collect();
    let blocked = needs.iter().filter(|(_, n)| *n == Need::Blocked).count();
    let waiting = needs.iter().filter(|(_, n)| queue::need_is_waiting(*n)).count();
    let working = needs.iter().filter(|(_, n)| *n == Need::Working).count();
    // Repaint the rail's per-project status badges from the reconciled needs.
    update_rail_badges(state, &needs);
    // Cheap hook-state hash (no transcript reads) so the dashboard refreshes when
    // an exact phase/hint changes without the badge counts moving. `0` when the
    // opt-in hooks aren't installed.
    let live_ids: Vec<String> = live.iter().map(|s| s.session_id.clone()).collect();
    let hook_fp = hooks::state_fingerprint(&live_ids);
    let statuses: HashMap<String, Status> =
        live.into_iter().map(|s| (s.session_id, s.status)).collect();

    let mut finished: Vec<(String, String)> = Vec::new();
    let mut refresh_dash = false;
    {
        let mut s = state.borrow_mut();
        update_queue_badge(&s.queue_badge, &s.queue_btn, waiting, working, blocked);
        // Keep the home overview live: when the waiting/working counts change
        // while it's the visible page, rebuild it (cheap — only on transitions,
        // not every tick) so it doesn't disagree with the header badge.
        let on_home = s.stack.visible_child_name().map(|n| n == "home").unwrap_or(false);
        if on_home && ((waiting, working) != s.last_dash_counts || hook_fp != s.last_dash_hook_fp) {
            s.last_dash_counts = (waiting, working);
            s.last_dash_hook_fp = hook_fp;
            refresh_dash = true;
        }
        let current = s.notebook.current_page();
        // Snapshot page numbers via cloned terminals so the mutable tabs loop
        // below doesn't also need to borrow s.notebook.
        let terminals: Vec<Terminal> = s.tabs.iter().map(|t| t.terminal.clone()).collect();
        let pages: Vec<Option<u32>> = terminals.iter().map(|t| s.notebook.page_num(t)).collect();

        for (i, tab) in s.tabs.iter_mut().enumerate() {
            let new_status = statuses.get(&tab.session_id).copied();
            if new_status == tab.last_status {
                continue; // nothing changed — don't churn the markup
            }
            set_status_dot(&tab.dot, new_status);
            // busy → idle on a background tab = it finished while you were away.
            if tab.last_status == Some(Status::Busy) && new_status == Some(Status::Idle) {
                let is_background = pages[i].is_none() || pages[i] != current;
                if is_background {
                    finished.push((
                        tab.session_id.clone(),
                        format!("{} · {}", basename(&tab.project_path), tab.title),
                    ));
                }
            }
            tab.last_status = new_status;
        }
    }
    for (sid, label) in finished {
        let notification = gio::Notification::new("Claude finished");
        notification.set_body(Some(&label));
        app.send_notification(Some(&format!("rune-done-{sid}")), &notification);
    }
    if refresh_dash {
        refresh_dashboard(state);
    }
}

/// Repaint the header queue badge (count of sessions waiting on you; hidden when
/// none) and its tooltip summary.
fn update_queue_badge(badge: &Label, btn: &Button, waiting: usize, working: usize, blocked: usize) {
    if waiting > 0 {
        badge.set_text(&waiting.to_string());
        badge.set_visible(true);
    } else {
        badge.set_visible(false);
    }
    // Tint the badge red when any session is blocked on a permission — the most
    // urgent state shouldn't read the same as a routine "your turn".
    if blocked > 0 {
        badge.add_css_class("queue-badge-blocked");
    } else {
        badge.remove_css_class("queue-badge-blocked");
    }
    let blocked_note = if blocked > 0 {
        format!(" · {blocked} blocked")
    } else {
        String::new()
    };
    btn.set_tooltip_text(Some(&format!(
        "Needs-you queue · {waiting} waiting · {working} working{blocked_note}"
    )));
}

// ───────────────────────────────────────────────────────────────────────────
// Needs-you queue (cross-project)
// ───────────────────────────────────────────────────────────────────────────

/// Pop the cross-project queue under the header button: every live session, the
/// ones needing you first. Clicking a row focuses its tab if open, else resumes
/// it (jump-in) — even for sessions started outside rune.
fn open_queue_popover(state: &State, anchor: &Button) {
    let entries = queue::build_queue();
    let waiting = queue::waiting_count(&entries);
    let working = queue::working_count(&entries);

    let content = gtk4::Box::new(Orientation::Vertical, 8);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(10);
    content.set_margin_end(10);
    content.set_size_request(400, -1);

    let heading = Label::builder().label("Needs you").xalign(0.0).build();
    heading.add_css_class("heading");
    content.append(&heading);

    let summary_text = if entries.is_empty() {
        "No Claude sessions are running right now.".to_string()
    } else {
        format!(
            "{waiting} waiting on you · {working} working · across all projects"
        )
    };
    let summary = Label::builder().label(summary_text).xalign(0.0).build();
    summary.add_css_class("dim-label");
    summary.add_css_class("caption");
    content.append(&summary);

    let popover = Popover::new();
    let listbox = ListBox::new();
    listbox.add_css_class("boxed-list");

    if entries.is_empty() {
        listbox.set_selection_mode(SelectionMode::None);
        let row = ListBoxRow::new();
        row.set_activatable(false);
        let empty = Label::builder()
            .label("Start a session, or run `claude` anywhere — it'll show up here.")
            .xalign(0.0)
            .wrap(true)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .build();
        empty.add_css_class("dim-label");
        row.set_child(Some(&empty));
        listbox.append(&row);
    } else {
        // Which of these live sessions rune actually hosts in a tab. For the
        // rest (started in some other terminal) rune can't attach to the running
        // pty — activating the row would `claude --resume` a *second* process on
        // the same id. We surface that, and refuse it outright for sessions that
        // are actively working (the worst case to duplicate).
        let open_ids: HashSet<String> = state
            .borrow()
            .tabs
            .iter()
            .map(|t| t.session_id.clone())
            .collect();
        for entry in &entries {
            let open_here = open_ids.contains(&entry.session_id);
            listbox.append(&make_queue_row(entry, open_here));
        }
        // index → (project, id, title), parallel to the rows appended above.
        let items: Vec<(String, String, String)> = entries
            .iter()
            .map(|e| (e.project_path.clone(), e.session_id.clone(), e.title.clone()))
            .collect();
        let st = state.clone();
        let pop = popover.clone();
        listbox.connect_row_activated(move |_, row| {
            if let Some((project, id, title)) = items.get(row.index() as usize) {
                resume_session(&st, project, id, title);
                pop.popdown();
            }
        });
    }

    let scroller = ScrolledWindow::builder()
        .child(&listbox)
        .hscrollbar_policy(PolicyType::Never)
        .max_content_height(460)
        .propagate_natural_height(true)
        .build();
    content.append(&scroller);

    popover.set_child(Some(&content));
    popover.set_parent(anchor);
    popover.set_position(PositionType::Bottom);
    popover.connect_closed(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));
    popover.popup();
}

fn make_queue_row(entry: &queue::QueueEntry, open_here: bool) -> ListBoxRow {
    let row = ListBoxRow::new();
    let hbox = gtk4::Box::new(Orientation::Horizontal, 8);
    hbox.set_margin_top(6);
    hbox.set_margin_bottom(6);
    hbox.set_margin_start(8);
    hbox.set_margin_end(8);

    let (color, need_text) = need_appearance(entry.need);
    let dot = Label::new(None);
    dot.set_valign(gtk4::Align::Center);
    dot.set_markup(&format!("<span foreground=\"{color}\">\u{25cf}</span>"));
    hbox.append(&dot);

    let vbox = gtk4::Box::new(Orientation::Vertical, 2);
    vbox.set_hexpand(true);
    let title = Label::builder()
        .label(&entry.title)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    let rel = sessions::relative_time_ms(entry.updated_at);
    let mut parts = vec![basename(&entry.project_path), need_text.to_string()];
    if !rel.is_empty() {
        parts.push(rel);
    }
    parts.push(format!("~{}", cost::format_usd(entry.cost_usd)));
    // The opt-in hook's activity hint (the task it's on, or the tool it wants),
    // when present — the same honest detail the command board shows.
    if let Some(h) = command_hint(entry.need, entry.hint.as_deref()) {
        parts.push(h);
    }
    // A session rune doesn't host is running in another terminal — clicking it
    // can't attach, only open a *new* view. Make that visible.
    if !open_here {
        parts.push("another terminal".to_string());
    }
    vbox.append(&title);

    // Meta line, led by the project's identity swatch so it reads as the same
    // project everywhere it appears. Normalize the path first: a queue entry's
    // project is the live session's raw cwd, while the rail/cards hash the
    // normalized config path — normalizing here keeps the colour identical.
    let meta = gtk4::Box::new(Orientation::Horizontal, 6);
    meta.append(&make_swatch(
        &config::normalize_project_path(&entry.project_path),
        7,
    ));
    let subtitle = Label::builder()
        .label(parts.join(" · "))
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    subtitle.add_css_class("dim-label");
    subtitle.add_css_class("caption");
    meta.append(&subtitle);
    vbox.append(&meta);
    hbox.append(&vbox);

    row.set_child(Some(&hbox));

    // Don't invite resuming a *second* process onto a session mid-turn in another
    // terminal — working or blocked-on-a-permission, there's no safe attach, so
    // it's read-only. Idle states stay openable.
    let external_unattachable =
        !open_here && matches!(entry.need, Need::Working | Need::Blocked);
    if external_unattachable {
        row.set_activatable(false);
        row.set_selectable(false);
        title.add_css_class("dim-label");
        row.set_tooltip_text(Some(
            "Mid-turn in another terminal — rune can't attach to a session it didn't start.",
        ));
    } else if open_here {
        row.set_tooltip_text(Some("Open in rune — jump to its tab."));
    } else {
        row.set_tooltip_text(Some(
            "Running in another terminal — opens a new view (rune can't attach to the live process).",
        ));
    }

    row
}

/// Dot color + label text for a queue entry's need-state.
fn need_appearance(need: Need) -> (&'static str, &'static str) {
    // Mission Control status language: green = working, amber = your turn,
    // periwinkle = just finished (the mockup's `--done`). Kept identical to
    // `set_status_dot` + the rail `.badge-*` so one colour means one thing
    // across the whole cockpit.
    match need {
        Need::Blocked => ("#ff6b6b", "Needs permission"),
        Need::Working => ("#43e08a", "Working…"),
        Need::JustFinished => ("#8ea2ff", "Just finished"),
        Need::YourTurn => ("#ffb13b", "Your turn"),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Home dashboard (the cross-project overview)
// ───────────────────────────────────────────────────────────────────────────

/// Toggle between the home overview and the open session terminals.
fn toggle_home(state: &State) {
    let on_home = state
        .borrow()
        .stack
        .visible_child_name()
        .map(|n| n == "home")
        .unwrap_or(false);
    if on_home {
        // Already home → go to the terminals (if any are open).
        let s = state.borrow();
        if s.notebook.n_pages() > 0 {
            s.stack.set_visible_child_name("sessions");
        }
    } else {
        show_home(state);
    }
}

/// Rebuild the dashboard from current data and show it.
fn show_home(state: &State) {
    refresh_dashboard(state);
    state.borrow().stack.set_visible_child_name("home");
}

/// Clear and rebuild the dashboard: a greeting, the cross-project needs-you
/// queue, project launch cards, and recent sessions — all from live data.
fn refresh_dashboard(state: &State) {
    let dash = state.borrow().dash_box.clone();
    while let Some(child) = dash.first_child() {
        dash.remove(&child);
    }

    // A new build → forget the previous build's card cost labels. A background
    // cost job spawned for the old build simply finds no label to fill in.
    let cost_cache = state.borrow().cost_cache.clone();
    let dash_cost_labels = state.borrow().dash_cost_labels.clone();
    let cost_in_flight = state.borrow().cost_in_flight.clone();
    dash_cost_labels.borrow_mut().clear();

    let entries = queue::build_queue();
    let waiting = queue::waiting_count(&entries);
    let working = queue::working_count(&entries);
    let blocked = queue::blocked_count(&entries);
    dash.append(&make_dash_greeting(waiting, working, blocked));

    // ── Needs you — the cross-project command board ──
    if !entries.is_empty() {
        let open_ids: HashSet<String> = state
            .borrow()
            .tabs
            .iter()
            .map(|t| t.session_id.clone())
            .collect();
        dash.append(&build_command_board(state, &entries, waiting, &open_ids));
    }

    // ── Projects (click a card → new session there) ──
    let projects = state.borrow().config.projects.clone();
    if !projects.is_empty() {
        dash.append(&dash_section_label("PROJECTS"));
        let flow = FlowBox::new();
        flow.set_selection_mode(SelectionMode::None);
        flow.set_min_children_per_line(1);
        flow.set_max_children_per_line(4);
        flow.set_homogeneous(true);
        flow.set_column_spacing(10);
        flow.set_row_spacing(10);

        // Per-project total cost is expensive (parses every transcript), so we
        // never compute it on this thread: show a cached value if the dir is
        // unchanged, else a placeholder + queue a background recompute.
        let mut to_compute: Vec<(String, std::path::PathBuf, u64)> = Vec::new();
        for project in &projects {
            let (card, cost_label) = make_dash_project_card(state, project);
            let dir = claude::project_transcript_dir(project);
            let fingerprint = sessions::dir_fingerprint(&dir);
            match cost_cache.borrow().get(project).copied() {
                Some((cached_fp, cost)) if cached_fp == fingerprint => set_card_cost(&cost_label, cost),
                _ => {
                    cost_label.set_text("·");
                    // Don't spawn a second worker for a project already being
                    // computed — its in-flight worker will fill in this build's
                    // label when it lands (dash_cost_labels is keyed by project).
                    if cost_in_flight.borrow_mut().insert(project.clone()) {
                        to_compute.push((project.clone(), dir, fingerprint));
                    }
                }
            }
            dash_cost_labels.borrow_mut().insert(project.clone(), cost_label);
            flow.append(&card);
        }
        dash.append(&flow);

        if !to_compute.is_empty() {
            spawn_cost_computation(to_compute, cost_cache, dash_cost_labels, cost_in_flight);
        }
    }

    // ── Recent sessions (click a row → resume) ──
    let recents = sessions::recent_sessions(&projects, 12);
    if !recents.is_empty() {
        dash.append(&dash_section_label("RECENT SESSIONS"));
        let list = ListBox::new();
        list.add_css_class("dash-list");
        list.set_selection_mode(SelectionMode::None);
        // Branch is a per-project file read — cache it so a project with several
        // recent rows only hits `.git/HEAD` once.
        let mut branches: HashMap<String, Option<String>> = HashMap::new();
        for (project, meta) in &recents {
            let branch = branches
                .entry(project.clone())
                .or_insert_with(|| git::current_branch(project))
                .clone();
            list.append(&make_dash_recent_row(project, meta, branch.as_deref()));
        }
        let items: Vec<(String, String, String)> = recents
            .iter()
            .map(|(p, m)| (p.clone(), m.id.clone(), m.title.clone()))
            .collect();
        let st = state.clone();
        list.connect_row_activated(move |_, row| {
            if let Some((project, id, title)) = items.get(row.index() as usize) {
                resume_session(&st, project, id, title);
            }
        });
        dash.append(&list);
    }

    // ── Footer totals ──
    dash.append(&make_dash_footer(&projects));
}

fn make_dash_greeting(waiting: usize, working: usize, blocked: usize) -> gtk4::Box {
    // The glance: a greeting headline on the left, status stat-pills on the right.
    let row = gtk4::Box::new(Orientation::Horizontal, 12);
    row.set_margin_bottom(8);

    let tod = match glib::DateTime::now_local().map(|d| d.hour()).unwrap_or(12) {
        0..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    };
    let greet = match greeting_name() {
        Some(name) => format!("{tod}, {name}"),
        None => tod.to_string(),
    };
    let headline = match (waiting, working) {
        (0, 0) => format!("{greet} — all quiet"),
        (0, _) => format!("{greet} — {working} working"),
        (1, _) => format!("{greet} — 1 agent wants you"),
        (w, _) => format!("{greet} — {w} agents want you"),
    };
    let title = Label::builder().label(headline).xalign(0.0).hexpand(true).build();
    title.add_css_class("dash-greeting");
    title.set_valign(gtk4::Align::Center);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    row.append(&title);

    let pills = gtk4::Box::new(Orientation::Horizontal, 10);
    pills.set_halign(gtk4::Align::End);
    pills.set_valign(gtk4::Align::Center);
    // Disjoint partition so the pills sum to the real session count: blocked
    // (most urgent, called out first), then the *rest* of the waiting sessions,
    // then working. `waiting` already includes blocked, so subtract it out.
    let plain_waiting = waiting.saturating_sub(blocked);
    if blocked > 0 {
        pills.append(&glance_pill("block", &format!("{blocked} blocked")));
    }
    if plain_waiting > 0 {
        pills.append(&glance_pill("turn", &format!("{plain_waiting} waiting")));
    }
    if working > 0 {
        pills.append(&glance_pill("work", &format!("{working} working")));
    }
    row.append(&pills);
    row
}

/// A stat-pill for the greeting glance: a coloured status dot + a count label.
/// `kind` is "block" (red, permission), "turn" (amber, your-turn) or "work"
/// (green, working).
fn glance_pill(kind: &str, text: &str) -> gtk4::Box {
    let pill = gtk4::Box::new(Orientation::Horizontal, 7);
    pill.add_css_class("glance-pill");
    pill.set_valign(gtk4::Align::Center);
    let dot = gtk4::Box::new(Orientation::Horizontal, 0);
    dot.add_css_class("glance-dot");
    dot.add_css_class(&format!("glance-dot-{kind}"));
    dot.set_size_request(7, 7);
    dot.set_valign(gtk4::Align::Center);
    let lbl = Label::new(Some(text));
    lbl.add_css_class("glance-pill-label");
    lbl.add_css_class("tnum");
    pill.append(&dot);
    pill.append(&lbl);
    pill
}

/// The cross-project **NEEDS YOU** command board: a banner over one rich row per
/// live session (status pill, title, project, time, cost, and an action button).
/// Each row is a `Button` wired to `resume_session`; a session working in another
/// terminal is shown but disabled (rune can't attach to a pty it didn't start).
fn build_command_board(
    state: &State,
    entries: &[queue::QueueEntry],
    waiting: usize,
    open_ids: &HashSet<String>,
) -> gtk4::Box {
    let board = gtk4::Box::new(Orientation::Vertical, 0);
    board.add_css_class("command-board");

    // Banner. The glyph sits in a framed accent chip (instrument-panel detail).
    let banner = gtk4::Box::new(Orientation::Horizontal, 12);
    banner.add_css_class("command-banner");
    let glyph_chip = gtk4::Box::new(Orientation::Horizontal, 0);
    glyph_chip.add_css_class("command-glyph-chip");
    glyph_chip.set_halign(gtk4::Align::Center);
    glyph_chip.set_valign(gtk4::Align::Center);
    let glyph = Image::from_icon_name("view-list-symbolic");
    glyph.add_css_class("command-glyph");
    glyph.set_halign(gtk4::Align::Center);
    glyph.set_valign(gtk4::Align::Center);
    glyph_chip.append(&glyph);
    banner.append(&glyph_chip);
    let btext = gtk4::Box::new(Orientation::Vertical, 2);
    btext.set_hexpand(true);
    let btitle = Label::builder().label("NEEDS YOU").xalign(0.0).build();
    btitle.add_css_class("command-title");
    // `waiting` counts both your-turn and just-finished sessions, so phrase the
    // subline as "waiting on you" (true for both — input or a review) rather than
    // overstating that they're all paused at a prompt.
    let bsub_text = if waiting > 0 {
        format!(
            "{waiting} {} waiting on you across all projects",
            if waiting == 1 { "agent" } else { "agents" }
        )
    } else {
        "Everything's running — nothing needs you yet.".to_string()
    };
    let bsub = Label::builder().label(bsub_text).xalign(0.0).build();
    bsub.add_css_class("command-sub");
    btext.append(&btitle);
    btext.append(&bsub);
    banner.append(&btext);
    board.append(&banner);

    for entry in entries {
        let open_here = open_ids.contains(&entry.session_id);
        board.append(&make_command_row(state, entry, open_here));
    }
    board
}

/// (pill css class, pill text, action-button label, action-is-primary) per need.
/// Pill text is UPPERCASE in the source (GTK CSS has no `text-transform`) to
/// match the locked mockup's instrument-panel pills.
fn command_appearance(need: Need) -> (&'static str, &'static str, &'static str, bool) {
    match need {
        // "Open" not "Approve": clicking jumps to the session's tab (where the
        // permission prompt lives) — rune can't answer the prompt for you.
        Need::Blocked => ("pill-block", "PERMISSION", "Open", true),
        Need::YourTurn => ("pill-turn", "YOUR TURN", "Open", true),
        Need::Working => ("pill-work", "WORKING…", "Watch", false),
        Need::JustFinished => ("pill-done", "FINISHED", "Review", false),
    }
}

/// A short key for the per-need row class (drives the left stripe colour in CSS).
fn need_key(need: Need) -> &'static str {
    match need {
        Need::Blocked => "block",
        Need::YourTurn => "turn",
        Need::Working => "work",
        Need::JustFinished => "done",
    }
}

/// Format the opt-in hook's activity hint for a row, framed by the need: a
/// blocked session *wants to use* a tool; a working one is *on* its task. Returns
/// `None` when there's no hint to show (hooks off, or nothing reported).
fn command_hint(need: Need, hint: Option<&str>) -> Option<String> {
    let h = hint?;
    Some(match need {
        Need::Blocked => format!("↳ wants to use {h}"),
        Need::Working => format!("↳ {h}"),
        // For an idle session the hint is the last task it was given — context,
        // not a live action.
        Need::YourTurn | Need::JustFinished => format!("↳ last: {h}"),
    })
}

/// One command-board row. The whole row is the click target (a `Button`); the
/// "action" cell is a button-styled label inside it, so clicking anywhere acts.
fn make_command_row(state: &State, entry: &queue::QueueEntry, open_here: bool) -> Button {
    let (pill_class, pill_text, action_label, action_primary) = command_appearance(entry.need);
    // A session rune doesn't host has no safe action when it's mid-turn: actively
    // *working* (a second `--resume` would duplicate a live turn) or *blocked* on
    // a permission (rune can't answer a prompt living in another terminal's TUI).
    // Idle states are still openable — you can pick the conversation back up.
    let external_unattachable =
        !open_here && matches!(entry.need, Need::Working | Need::Blocked);
    // A session rune HOSTS and that's waiting for your next prompt can be replied
    // to straight from the dashboard: rune owns its PTY, so it can feed your
    // message in. NOT a `Blocked` session — it's on a permission menu that wants a
    // specific keypress (1/2/y), not free-text prose, so that row keeps "Open" to
    // answer in the TUI. NOT a working one (busy) or "another terminal" (no PTY).
    let repliable = open_here && matches!(entry.need, Need::YourTurn | Need::JustFinished);
    let action_label = if repliable { "Reply" } else { action_label };
    let action_primary = action_primary || repliable;

    let row = Button::new();
    row.add_css_class("command-row");
    row.add_css_class(&format!("row-{}", need_key(entry.need)));

    let hbox = gtk4::Box::new(Orientation::Horizontal, 14);

    // Left status stripe (full-height accent bar).
    let stripe = gtk4::Box::new(Orientation::Horizontal, 0);
    stripe.add_css_class("cmd-stripe");
    stripe.set_size_request(3, -1);
    stripe.set_valign(gtk4::Align::Fill);
    hbox.append(&stripe);

    // Status pill (fixed column so the bodies line up).
    let pill = gtk4::Box::new(Orientation::Horizontal, 7);
    pill.add_css_class("cmd-pill");
    pill.add_css_class(pill_class);
    pill.set_halign(gtk4::Align::Start);
    pill.set_valign(gtk4::Align::Center);
    let pdot = gtk4::Box::new(Orientation::Horizontal, 0);
    pdot.add_css_class("cmd-pd");
    pdot.set_size_request(8, 8);
    pdot.set_valign(gtk4::Align::Center);
    let plabel = Label::new(Some(pill_text));
    plabel.add_css_class("cmd-pill-label");
    pill.append(&pdot);
    pill.append(&plabel);
    let pill_col = gtk4::Box::new(Orientation::Horizontal, 0);
    pill_col.set_size_request(116, -1);
    pill_col.set_valign(gtk4::Align::Center);
    pill_col.append(&pill);
    hbox.append(&pill_col);

    // Body: title + meta (project swatch + name · time · "another terminal").
    let body = gtk4::Box::new(Orientation::Vertical, 3);
    body.set_hexpand(true);
    body.set_valign(gtk4::Align::Center);
    let title = Label::builder()
        .label(&entry.title)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    title.add_css_class("cmd-title");
    body.append(&title);

    let meta = gtk4::Box::new(Orientation::Horizontal, 7);
    meta.append(&make_swatch(
        &config::normalize_project_path(&entry.project_path),
        8,
    ));
    let pj = Label::new(Some(&basename(&entry.project_path)));
    pj.add_css_class("dash-pj");
    pj.add_css_class("caption");
    meta.append(&pj);
    let rel = sessions::relative_time_ms(entry.updated_at);
    let mut tail = String::new();
    if !rel.is_empty() {
        tail.push_str(&format!(" · {rel}"));
    }
    if !open_here {
        tail.push_str(" · another terminal");
    }
    if !tail.is_empty() {
        let m2 = Label::new(Some(&tail));
        m2.add_css_class("dim-label");
        m2.add_css_class("caption");
        meta.append(&m2);
    }
    body.append(&meta);

    // Activity hint from the opt-in hooks: the task it's working on, or the tool
    // it wants permission for. Honest — only shown when a hook actually reported
    // it; otherwise this line is absent (no fabricated "editing X").
    if let Some(h) = command_hint(entry.need, entry.hint.as_deref()) {
        let hint = Label::builder()
            .label(&h)
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        hint.add_css_class("cmd-hint");
        hint.add_css_class("caption");
        body.append(&hint);
    }
    hbox.append(&body);

    // Right: cost + the action affordance.
    let right = gtk4::Box::new(Orientation::Horizontal, 14);
    right.set_valign(gtk4::Align::Center);
    let cost = Label::new(Some(&format!("~{}", cost::format_usd(entry.cost_usd))));
    cost.add_css_class("cmd-cost");
    cost.add_css_class("tnum");
    right.append(&cost);
    // A mid-turn session in another terminal has no action rune can safely take
    // (it can't attach), so we DON'T draw an action button there — a dead
    // "Watch"/"Approve" that does nothing (and whose tooltip a disabled widget
    // won't even show) would mislead. The dimmed row + "· another terminal" crumb
    // say it.
    if !external_unattachable {
        let action = Label::new(Some(action_label));
        action.add_css_class("cmd-action");
        if action_primary {
            action.add_css_class("cmd-action-primary");
        }
        right.append(&action);
    }
    hbox.append(&right);

    row.set_child(Some(&hbox));

    if external_unattachable {
        row.set_sensitive(false);
    } else if repliable {
        let st = state.clone();
        let id = entry.session_id.clone();
        let title_s = entry.title.clone();
        row.connect_clicked(move |btn| {
            let parent = btn.root().and_downcast::<gtk4::Window>();
            open_reply(&st, &id, &title_s, parent.as_ref());
        });
        row.set_tooltip_text(Some("Reply — send a message to this session from here."));
    } else {
        let st = state.clone();
        let project = entry.project_path.clone();
        let id = entry.session_id.clone();
        let title_s = entry.title.clone();
        row.connect_clicked(move |_| resume_session(&st, &project, &id, &title_s));
        row.set_tooltip_text(Some(if open_here {
            "Open in rune — jump to its tab."
        } else {
            "Open a new view (rune can't attach to a process it didn't start)."
        }));
    }
    row
}

/// Send `text` (plus Enter) into a rune-hosted session's terminal, then jump to
/// its tab. Returns `true` if it was actually delivered. No-op (returns `false`)
/// if the message is blank, the tab is gone, or — crucially — `claude` is no
/// longer the live process for that session.
fn send_to_session(state: &State, session_id: &str, text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let term = {
        let s = state.borrow();
        s.tabs
            .iter()
            .find(|t| t.session_id == session_id)
            .map(|t| t.terminal.clone())
    };
    let Some(term) = term else {
        return false; // tab closed between opening the modal and sending
    };
    // CRUCIAL: a tab's PTY runs `claude …; exec bash -i`, so if claude has exited
    // the foreground program is now a SHELL — feeding prose there would run it as
    // a command. Only feed when claude is still the live process for this session
    // (the same liveness the queue uses: the session's pid is in /proc).
    let claude_live = status::live_sessions()
        .iter()
        .any(|ls| ls.session_id == session_id);
    if !claude_live {
        return false;
    }
    // Feed the message followed by a carriage return — exactly what typing it and
    // pressing Enter sends down the PTY. It's the user's own input to their own
    // live claude session, so no sanitizing beyond the trim above.
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(b'\r');
    term.feed_child(&bytes);
    // Jump to the session so the user sees the message land + the reply.
    let s = state.borrow();
    if let Some(page) = s.notebook.page_num(&term) {
        s.notebook.set_current_page(Some(page));
    }
    s.stack.set_visible_child_name("sessions");
    drop(s);
    term.grab_focus();
    true
}

/// A small modal to fire a one-line message into a hosted session without leaving
/// the dashboard. Frameless like the other rune modals (avoids the GL-renderer
/// server-side-frame teardown race — see `open_settings`).
fn open_reply(state: &State, session_id: &str, title: &str, parent: Option<&gtk4::Window>) {
    let win = Window::builder()
        .modal(true)
        .decorated(false)
        .default_width(440)
        .resizable(false)
        .build();
    if let Some(p) = parent {
        win.set_transient_for(Some(p));
    }
    win.set_destroy_with_parent(true);
    win.add_css_class("rune-panel");

    let outer = gtk4::Box::new(Orientation::Vertical, 12);
    outer.set_margin_top(18);
    outer.set_margin_bottom(18);
    outer.set_margin_start(18);
    outer.set_margin_end(18);

    let heading = Label::builder()
        .label(format!("Reply — {title}"))
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .build();
    heading.add_css_class("title-4");
    outer.append(&heading);
    let hint = Label::builder()
        .label("Sends your message into this session, then opens its tab.")
        .xalign(0.0)
        .wrap(true)
        .build();
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    outer.append(&hint);

    let entry = Entry::builder()
        .placeholder_text("Type a message and press Enter")
        .activates_default(false)
        .build();
    outer.append(&entry);

    let buttons = gtk4::Box::new(Orientation::Horizontal, 8);
    buttons.set_halign(gtk4::Align::End);
    buttons.set_margin_top(6);
    let cancel = Button::with_label("Cancel");
    let send = Button::with_label("Send");
    send.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&send);
    outer.append(&buttons);

    let do_send = Rc::new({
        let st = state.clone();
        let id = session_id.to_string();
        let w = win.clone();
        let entry = entry.clone();
        move || {
            // Only dismiss the modal if the message was actually delivered. An
            // empty entry, a closed tab, or an exited claude leaves it open so the
            // user's text isn't silently swallowed.
            if send_to_session(&st, &id, &entry.text()) {
                w.close();
            }
        }
    });
    {
        let do_send = do_send.clone();
        entry.connect_activate(move |_| do_send());
    }
    {
        let do_send = do_send.clone();
        send.connect_clicked(move |_| do_send());
    }
    {
        let w = win.clone();
        cancel.connect_clicked(move |_| w.close());
    }
    {
        let w = win.clone();
        let key = EventControllerKey::new();
        key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, keyval, _code, _mods| {
            if keyval == gdk::Key::Escape {
                w.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        win.add_controller(key);
    }

    win.set_child(Some(&outer));
    win.present();
    entry.grab_focus();
}

/// The dashboard footer: the wordmark + project/session totals. A grand-total
/// cost across projects is intentionally *not* shown here — per-project costs
/// already live on the cards, and a footer sum would either go stale against
/// them (each session appends to its transcript every turn, flipping the cached
/// fingerprint) or seldom appear, so it isn't worth the inconsistency.
fn make_dash_footer(projects: &[String]) -> gtk4::Box {
    let foot = gtk4::Box::new(Orientation::Horizontal, 10);
    foot.add_css_class("dash-foot");
    foot.set_margin_top(10);

    let wm = Label::new(None);
    wm.set_markup("rune<span foreground=\"#36d3fa\">_</span>");
    wm.add_css_class("foot-wm");
    foot.append(&wm);

    let nproj = projects.len();
    let nsess: usize = projects.iter().map(|p| sessions::session_count(p)).sum();
    let summary = format!(
        "· {nproj} project{} · {nsess} session{} tracked",
        plural(nproj),
        plural(nsess)
    );
    let lbl = Label::builder().label(summary).xalign(0.0).hexpand(true).build();
    lbl.add_css_class("foot-text");
    foot.append(&lbl);
    foot
}

/// A clean first name for the greeting, or `None` when the system only has a
/// login id (we'd rather say "Good evening" than "Good evening, user").
fn greeting_name() -> Option<String> {
    let real = glib::real_name();
    let real = real.to_string_lossy();
    let first = real.split_whitespace().next()?;
    if first.len() >= 2 && first.chars().all(|c| c.is_alphabetic()) {
        let mut chars = first.chars();
        let head = chars.next()?.to_uppercase().collect::<String>();
        Some(head + chars.as_str())
    } else {
        None
    }
}

fn dash_section_label(text: &str) -> Label {
    let label = Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("dash-section");
    label
}

/// Select a project (syncing the sidebar highlight + the New/Resume toolbar
/// buttons) and open a fresh Claude session in it. Shared by the dashboard
/// project cards and the Ctrl-K switcher so both keep the rest of the UI in sync.
fn launch_new_session(state: &State, project: &str) {
    state.borrow_mut().selected_project = Some(project.to_string());
    rebuild_sidebar(state); // syncs the sidebar selection + enables the toolbar
    add_tab(state, project, claude::new_session_id(), None, true);
    save_state(state);
}

/// A project launch card. Returns the card plus its cost `Label`, which starts
/// as a placeholder and is filled in by [`spawn_cost_computation`] (or straight
/// away from the cache) — per-project cost is too heavy to compute inline.
fn make_dash_project_card(state: &State, project: &str) -> (Button, Label) {
    let card = Button::new();
    card.add_css_class("dash-card");
    card.set_hexpand(true);
    card.set_tooltip_text(Some(&format!(
        "Start a new Claude session in {}",
        basename(project)
    )));

    let vbox = gtk4::Box::new(Orientation::Vertical, 6);
    let top = gtk4::Box::new(Orientation::Horizontal, 8);
    top.append(&make_swatch(project, 9));
    let name = Label::builder()
        .label(basename(project))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .build();
    name.add_css_class("dash-card-name");
    top.append(&name);
    vbox.append(&top);

    let row = gtk4::Box::new(Orientation::Horizontal, 8);
    let count = sessions::session_count(project);
    let sub = Label::builder()
        .label(format!("{count} session{}", if count == 1 { "" } else { "s" }))
        .xalign(0.0)
        .hexpand(true)
        .build();
    sub.add_css_class("dim-label");
    sub.add_css_class("caption");
    let cost = Label::builder().label("·").xalign(1.0).build();
    cost.add_css_class("dash-card-cost");
    cost.add_css_class("tnum");
    cost.set_tooltip_text(Some(COST_ESTIMATE_TOOLTIP));
    row.append(&sub);
    row.append(&cost);
    vbox.append(&row);

    card.set_child(Some(&vbox));

    {
        let st = state.clone();
        let project = project.to_string();
        card.connect_clicked(move |_| launch_new_session(&st, &project));
    }
    (card, cost)
}

/// Shared caveat for every estimated-cost figure, so all call sites stay in sync.
const COST_ESTIMATE_TOOLTIP: &str =
    "Estimated total across all of this project's sessions — from public per-model \
     token rates; excludes batch/priority pricing and may drift.";

/// Show a finished per-project cost on a card's cost label. The `~` marks it as
/// an estimate, matching every other dollar figure in the app.
fn set_card_cost(label: &Label, cost: f64) {
    label.set_text(&format!("~{}", cost::format_usd(cost)));
}

/// Compute per-project total cost off the UI thread (it parses every transcript
/// — hundreds of MB) and fill in each card's cost label as results arrive. The
/// worker is pure file/JSON work (no GTK/GLib); a short main-thread timeout
/// drains its channel, updates the cache, and ends once the worker is done.
/// `cost_in_flight` (main-thread only) records which projects are mid-compute so
/// refresh_dashboard never spawns a duplicate worker for the same project.
fn spawn_cost_computation(
    to_compute: Vec<(String, std::path::PathBuf, u64)>,
    cost_cache: Rc<RefCell<HashMap<String, (u64, f64)>>>,
    dash_cost_labels: Rc<RefCell<HashMap<String, Label>>>,
    cost_in_flight: Rc<RefCell<HashSet<String>>>,
) {
    // Projects this worker owns — used to clear in-flight stragglers if the
    // worker dies before reporting every one (it normally reports each).
    let owned: Vec<String> = to_compute.iter().map(|(p, _, _)| p.clone()).collect();
    let (tx, rx) = std::sync::mpsc::channel::<(String, u64, f64)>();
    std::thread::spawn(move || {
        for (project, dir, fingerprint) in to_compute {
            let cost = sessions::dir_total_cost(&dir);
            // Receiver gone (window closed) → stop.
            if tx.send((project, fingerprint, cost)).is_err() {
                break;
            }
        }
    });
    glib::timeout_add_local(std::time::Duration::from_millis(120), move || loop {
        match rx.try_recv() {
            Ok((project, fingerprint, cost)) => {
                cost_cache
                    .borrow_mut()
                    .insert(project.clone(), (fingerprint, cost));
                cost_in_flight.borrow_mut().remove(&project);
                // The label map is rebuilt each dashboard build; if this project
                // still has a card on screen, fill its cost in.
                if let Some(label) = dash_cost_labels.borrow().get(&project) {
                    set_card_cost(label, cost);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Worker done (or gone) — clear any projects it never reported so
                // they aren't stuck "in flight" and can be recomputed next build.
                let mut flight = cost_in_flight.borrow_mut();
                for p in &owned {
                    flight.remove(p);
                }
                return glib::ControlFlow::Break;
            }
        }
    });
}

fn make_dash_recent_row(project: &str, meta: &SessionMeta, branch: Option<&str>) -> ListBoxRow {
    let row = ListBoxRow::new();
    let hbox = gtk4::Box::new(Orientation::Horizontal, 14);
    hbox.set_margin_top(8);
    hbox.set_margin_bottom(8);
    hbox.set_margin_start(12);
    hbox.set_margin_end(12);

    // ── left: title + meta line (project · branch · prompts) ──
    let vbox = gtk4::Box::new(Orientation::Vertical, 3);
    vbox.set_hexpand(true);
    let title = Label::builder()
        .label(&meta.title)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    vbox.append(&title);

    let meta_row = gtk4::Box::new(Orientation::Horizontal, 7);
    meta_row.append(&make_swatch(project, 7));
    let pj = Label::new(Some(&basename(project)));
    pj.add_css_class("dash-pj");
    pj.add_css_class("caption");
    meta_row.append(&pj);
    if let Some(b) = branch {
        meta_row.append(&dash_midot());
        meta_row.append(&branch_widget(b));
    }
    meta_row.append(&dash_midot());
    let plural = if meta.prompt_count == 1 { "prompt" } else { "prompts" };
    let prompts = Label::new(Some(&format!("{} {}", meta.prompt_count, plural)));
    prompts.add_css_class("dim-label");
    prompts.add_css_class("caption");
    prompts.add_css_class("tnum");
    meta_row.append(&prompts);
    vbox.append(&meta_row);
    hbox.append(&vbox);

    // ── model chip (fixed column so rows align even when a model is missing;
    //    clipped so an abnormally long model id can't push the other columns) ──
    let chip_col = gtk4::Box::new(Orientation::Horizontal, 0);
    chip_col.set_size_request(104, -1);
    chip_col.set_halign(gtk4::Align::Start);
    chip_col.set_valign(gtk4::Align::Center);
    chip_col.set_overflow(gtk4::Overflow::Hidden);
    if !meta.model.is_empty() {
        chip_col.append(&model_chip(&meta.model));
    }
    hbox.append(&chip_col);

    // ── context-fill bar ──
    let ctx_col = gtk4::Box::new(Orientation::Vertical, 0);
    ctx_col.set_size_request(108, -1);
    ctx_col.set_valign(gtk4::Align::Center);
    if let Some(w) = context_widget(meta.context_tokens, &meta.model) {
        ctx_col.append(&w);
    }
    hbox.append(&ctx_col);

    // ── cost over relative time ──
    let cost_col = gtk4::Box::new(Orientation::Vertical, 2);
    cost_col.set_size_request(72, -1);
    cost_col.set_halign(gtk4::Align::End);
    cost_col.set_valign(gtk4::Align::Center);
    let amt = Label::builder()
        .label(format!("~{}", cost::format_usd(meta.cost_usd)))
        .xalign(1.0)
        .build();
    amt.add_css_class("scost-amt");
    amt.add_css_class("tnum");
    let when = Label::builder()
        .label(sessions::relative_time(meta.modified))
        .xalign(1.0)
        .build();
    when.add_css_class("scost-when");
    cost_col.append(&amt);
    cost_col.append(&when);
    hbox.append(&cost_col);

    row.set_child(Some(&hbox));
    row
}

/// The "·" separator between meta-line fields.
fn dash_midot() -> Label {
    let l = Label::new(Some("·"));
    l.add_css_class("dash-midot");
    l
}

/// A compact branch tag, e.g. "⎇ main".
fn branch_widget(branch: &str) -> Label {
    let l = Label::new(Some(&format!("\u{2387} {branch}")));
    l.add_css_class("branch-chip");
    l.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    l.set_max_width_chars(18);
    l.set_tooltip_text(Some(&format!("git branch: {branch}")));
    l
}

/// A model chip — a colored dot + short model name, tinted per family.
fn model_chip(model: &str) -> gtk4::Box {
    let chip = gtk4::Box::new(Orientation::Horizontal, 6);
    chip.add_css_class("chip");
    chip.add_css_class(&format!("chip-{}", cost::model_family(model)));
    chip.set_halign(gtk4::Align::Start);
    chip.set_valign(gtk4::Align::Center);
    let dot = gtk4::Box::new(Orientation::Horizontal, 0);
    dot.add_css_class("chip-dot");
    dot.set_size_request(6, 6);
    dot.set_valign(gtk4::Align::Center);
    let label = Label::new(Some(&cost::model_label(model)));
    // Bound the chip text (like branch_widget) so a malformed/oversized model id
    // can't widen the chip past its column.
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_max_width_chars(14);
    chip.append(&dot);
    chip.append(&label);
    chip
}

/// A two-line context-fill indicator: "CTX  48%" over a colored bar, or `None`
/// when the session has no assistant turn yet. The tooltip shows the exact token
/// count and the assumed window, since the percentage is an estimate.
fn context_widget(tokens: u64, model: &str) -> Option<gtk4::Box> {
    let pct = cost::context_pct(tokens, model)?;
    let v = gtk4::Box::new(Orientation::Vertical, 3);

    let top = gtk4::Box::new(Orientation::Horizontal, 0);
    let lbl = Label::new(Some("CTX"));
    lbl.add_css_class("ctx-lbl");
    lbl.set_hexpand(true);
    lbl.set_xalign(0.0);
    let val = Label::new(Some(&format!("{pct}%")));
    val.add_css_class("ctx-val");
    val.add_css_class("tnum");
    top.append(&lbl);
    top.append(&val);

    // A plain track + fill (not a ProgressBar — its progress gizmo reports a
    // negative min-width and spams GTK warnings). The column is a fixed width,
    // so a fixed-width track is fine and gives us a crisp thin bar.
    const TRACK_W: i32 = 96;
    let level = match pct {
        0..=49 => "lvl-lo",
        50..=79 => "lvl-mid",
        _ => "lvl-hi",
    };
    let track = gtk4::Box::new(Orientation::Horizontal, 0);
    track.add_css_class("ctx-track");
    track.set_size_request(TRACK_W, 5);
    track.set_halign(gtk4::Align::Fill);
    track.set_overflow(gtk4::Overflow::Hidden);
    let fill = gtk4::Box::new(Orientation::Horizontal, 0);
    fill.add_css_class("ctx-fill");
    fill.add_css_class(level);
    let fill_w = ((TRACK_W as f64 * pct as f64 / 100.0).round() as i32).clamp(2, TRACK_W);
    fill.set_size_request(fill_w, 5);
    fill.set_halign(gtk4::Align::Start);
    track.append(&fill);

    v.append(&top);
    v.append(&track);
    v.set_tooltip_text(Some(&format!(
        "~{} of {} context tokens (window assumed from the model — estimated)",
        fmt_tokens(tokens),
        fmt_tokens(cost::context_window(model))
    )));
    Some(v)
}

/// Compact token count for tooltips: 782911 → "783K", 1000000 → "1.0M".
fn fmt_tokens(t: u64) -> String {
    if t >= 1_000_000 {
        format!("{:.1}M", t as f64 / 1_000_000.0)
    } else if t >= 1_000 {
        format!("{}K", (t as f64 / 1000.0).round() as u64)
    } else {
        t.to_string()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Global quick-switcher (Ctrl-K)
// ───────────────────────────────────────────────────────────────────────────

/// What activating a palette row does.
enum PaletteAction {
    /// Start a fresh session in a project.
    NewSession { project: String },
    /// Resume (or focus) a past session.
    Resume {
        project: String,
        id: String,
        title: String,
    },
}

struct PaletteItem {
    label: String,
    subtitle: String,
    /// Lower-cased text the fuzzy filter matches against (title + project).
    haystack: String,
    action: PaletteAction,
}

/// A fuzzy command palette over every project (→ new session) and the most
/// recently active sessions across all projects (→ resume). Type to filter,
/// ↑/↓ to move, Enter to jump, Esc to dismiss.
fn open_quick_switcher(state: &State, parent: &ApplicationWindow) {
    let items = Rc::new(build_palette_items(state));

    // Non-modal so a click on the cockpit light-dismisses it (matching the
    // resume/queue popovers); a focus-leave handler below does the closing.
    let win = Window::builder()
        .transient_for(parent)
        .modal(false)
        .decorated(false)
        .default_width(580)
        .default_height(440)
        .build();
    win.set_destroy_with_parent(true);
    win.add_css_class("rune-panel");

    let entry = Entry::builder()
        .placeholder_text("Jump to a project or session…  (Esc to close)")
        .primary_icon_name("system-search-symbolic")
        .build();

    let listbox = ListBox::new();
    listbox.set_selection_mode(SelectionMode::Single);
    listbox.add_css_class("boxed-list");

    let scroller = ScrolledWindow::builder()
        .child(&listbox)
        .vexpand(true)
        .hscrollbar_policy(PolicyType::Never)
        .build();

    let vbox = gtk4::Box::new(Orientation::Vertical, 8);
    vbox.set_margin_top(10);
    vbox.set_margin_bottom(10);
    vbox.set_margin_start(10);
    vbox.set_margin_end(10);
    vbox.append(&entry);
    vbox.append(&scroller);
    win.set_child(Some(&vbox));

    // visible[row index] → items index (the filtered, ranked mapping).
    let visible = Rc::new(RefCell::new(Vec::<usize>::new()));

    let repopulate: Rc<dyn Fn(&str)> = {
        let items = items.clone();
        let visible = visible.clone();
        let listbox = listbox.clone();
        Rc::new(move |query: &str| {
            while let Some(child) = listbox.first_child() {
                listbox.remove(&child);
            }
            let q = query.trim().to_lowercase();
            let mut scored: Vec<(i32, usize)> = Vec::new();
            for (i, item) in items.iter().enumerate() {
                if let Some(score) = fuzzy_score(&q, &item.haystack) {
                    scored.push((score, i));
                }
            }
            // Empty query keeps natural order (projects, then recent sessions);
            // a query ranks best-match first (stable sort preserves order on ties).
            if !q.is_empty() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            let mut vis = visible.borrow_mut();
            vis.clear();
            for (_, i) in &scored {
                listbox.append(&make_palette_row(&items[*i]));
                vis.push(*i);
            }
            drop(vis);
            if scored.is_empty() {
                // Explicit empty state, matching the resume picker — a blank box
                // shouldn't read as broken. The row carries no item (not in
                // `visible`), so Enter safely no-ops.
                let row = ListBoxRow::new();
                row.set_activatable(false);
                row.set_selectable(false);
                let label = Label::builder()
                    .label("No matches")
                    .xalign(0.0)
                    .margin_top(8)
                    .margin_bottom(8)
                    .margin_start(8)
                    .margin_end(8)
                    .build();
                label.add_css_class("dim-label");
                row.set_child(Some(&label));
                listbox.append(&row);
            } else if let Some(row) = listbox.row_at_index(0) {
                listbox.select_row(Some(&row));
            }
        })
    };
    repopulate("");

    {
        let repopulate = repopulate.clone();
        entry.connect_changed(move |e| repopulate(&e.text()));
    }

    // Activate the selected/clicked row: perform its action and dismiss.
    let activate_row = {
        let items = items.clone();
        let visible = visible.clone();
        let st = state.clone();
        let win = win.clone();
        Rc::new(move |idx: i32| {
            if idx < 0 {
                return;
            }
            if let Some(&item_i) = visible.borrow().get(idx as usize) {
                perform_palette_action(&st, &items[item_i].action);
                win.close();
            }
        })
    };
    {
        let activate_row = activate_row.clone();
        listbox.connect_row_activated(move |_, row| activate_row(row.index()));
    }
    {
        // Enter in the search entry activates the highlighted row.
        let activate_row = activate_row.clone();
        let listbox = listbox.clone();
        entry.connect_activate(move |_| {
            let idx = listbox.selected_row().map(|r| r.index()).unwrap_or(-1);
            activate_row(idx);
        });
    }

    // ↑/↓ move the selection (keeping focus in the entry so you keep typing);
    // Esc dismisses. Capture phase so the entry doesn't eat the arrows first.
    {
        let win_close = win.clone();
        let lb = listbox.clone();
        let ent = entry.clone();
        let key = EventControllerKey::new();
        key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, keyval, _code, _mods| match keyval {
            gdk::Key::Escape => {
                win_close.close();
                glib::Propagation::Stop
            }
            gdk::Key::Down => {
                switcher_move(&lb, &ent, 1);
                glib::Propagation::Stop
            }
            gdk::Key::Up => {
                switcher_move(&lb, &ent, -1);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        win.add_controller(key);
    }

    // Light-dismiss: close as soon as the switcher stops being the active window
    // (a click on the cockpit, an alt-tab away) — the popover-like behavior users
    // expect here. The initial inactive→active transition on present keeps it up.
    win.connect_is_active_notify(move |w| {
        if !w.is_active() {
            w.close();
        }
    });

    win.present();
    entry.grab_focus();
}

/// Move the switcher selection by `delta`, scrolling the row into view but
/// returning focus to the entry so typing continues uninterrupted.
fn switcher_move(listbox: &ListBox, entry: &Entry, delta: i32) {
    let cur = listbox.selected_row().map(|r| r.index()).unwrap_or(-1);
    let next = cur + delta;
    if next < 0 {
        return;
    }
    if let Some(row) = listbox.row_at_index(next) {
        listbox.select_row(Some(&row));
        row.grab_focus(); // pulls it into view in the ScrolledWindow
        entry.grab_focus(); // …then hand typing back to the search box
    }
}

fn build_palette_items(state: &State) -> Vec<PaletteItem> {
    let projects = state.borrow().config.projects.clone();
    let mut items = Vec::new();

    // Every project → start a new session there.
    for project in &projects {
        let base = basename(project);
        items.push(PaletteItem {
            haystack: format!("{base} {project}").to_lowercase(),
            label: base,
            subtitle: format!("New session · {project}"),
            action: PaletteAction::NewSession {
                project: project.clone(),
            },
        });
    }

    // Recent sessions across all projects → resume (or focus if already open).
    for (project, meta) in sessions::recent_sessions(&projects, sessions::SWITCHER_RECENT) {
        let base = basename(&project);
        let plural = if meta.prompt_count == 1 { "prompt" } else { "prompts" };
        items.push(PaletteItem {
            haystack: format!("{} {base}", meta.title).to_lowercase(),
            label: meta.title.clone(),
            subtitle: format!(
                "{base} · {} · {} {plural} · ~{}",
                sessions::relative_time(meta.modified),
                meta.prompt_count,
                cost::format_usd(meta.cost_usd),
            ),
            action: PaletteAction::Resume {
                project,
                id: meta.id,
                title: meta.title,
            },
        });
    }

    items
}

fn make_palette_row(item: &PaletteItem) -> ListBoxRow {
    let row = ListBoxRow::new();
    let vbox = gtk4::Box::new(Orientation::Vertical, 2);
    vbox.set_margin_top(6);
    vbox.set_margin_bottom(6);
    vbox.set_margin_start(8);
    vbox.set_margin_end(8);

    let label = Label::builder()
        .label(&item.label)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    let subtitle = Label::builder()
        .label(&item.subtitle)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    subtitle.add_css_class("dim-label");
    subtitle.add_css_class("caption");

    vbox.append(&label);
    vbox.append(&subtitle);
    row.set_child(Some(&vbox));
    row
}

fn perform_palette_action(state: &State, action: &PaletteAction) {
    match action {
        PaletteAction::NewSession { project } => {
            launch_new_session(state, project);
        }
        PaletteAction::Resume { project, id, title } => {
            resume_session(state, project, id, title);
        }
    }
}

/// Case-insensitive subsequence fuzzy match. `None` if `query` isn't a
/// subsequence of `text`; otherwise a score that rewards consecutive and
/// word-start matches and lightly penalizes longer haystacks. `query` is
/// expected already lower-cased; an empty query always matches (score 0).
fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut qi = 0;
    let mut score = 0;
    let mut last_match: i32 = -2;
    for (ti, &tc) in t.iter().enumerate() {
        if qi < q.len() && tc == q[qi] {
            if ti as i32 == last_match + 1 {
                score += 5; // consecutive run
            } else {
                score += 1;
            }
            if ti == 0 || !t[ti - 1].is_alphanumeric() {
                score += 3; // start of a word
            }
            last_match = ti as i32;
            qi += 1;
        }
    }
    if qi == q.len() {
        Some(score - (t.len() as i32) / 10)
    } else {
        None
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Settings
// ───────────────────────────────────────────────────────────────────────────

/// Per-project launch preset labels → values. Index 0 is "default" (`None`, don't
/// pass the flag); the rest map to `claude` CLI flag values.
const PRESET_MODEL_LABELS: [&str; 5] =
    ["Use default", "Opus — smartest", "Sonnet — faster", "Haiku — fastest", "Fable"];
const PRESET_MODEL_VALUES: [Option<&str>; 5] =
    [None, Some("opus"), Some("sonnet"), Some("haiku"), Some("fable")];
const PRESET_PERM_LABELS: [&str; 4] = [
    "Ask before changes",
    "Accept edits automatically",
    "Plan first (no changes)",
    "Bypass all checks — skips every prompt",
];
const PRESET_PERM_VALUES: [Option<&str>; 4] =
    [None, Some("acceptEdits"), Some("plan"), Some("bypassPermissions")];

/// Per-project launch settings: which `--model` and `--permission-mode` rune
/// starts this project's sessions with. Saved to the project's preset and applied
/// to sessions opened from now on (a running session keeps the flags it started
/// with — claude reads them at launch). Frameless like the other modals so the
/// GL-renderer teardown can't race a server-side frame (see `open_settings`).
fn open_project_preset(state: &State, path: &str, parent: Option<&gtk4::Window>) {
    let cur = state.borrow().config.preset_for(path);

    let win = Window::builder()
        .modal(true)
        .decorated(false)
        .default_width(420)
        .resizable(false)
        .build();
    if let Some(p) = parent {
        win.set_transient_for(Some(p));
    }
    win.set_destroy_with_parent(true);
    win.add_css_class("rune-panel");

    let outer = gtk4::Box::new(Orientation::Vertical, 12);
    outer.set_margin_top(18);
    outer.set_margin_bottom(18);
    outer.set_margin_start(18);
    outer.set_margin_end(18);

    let title = Label::builder()
        .label(format!("Launch settings — {}", basename(path)))
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::Middle)
        .build();
    title.add_css_class("title-4");
    outer.append(&title);
    let hint = Label::builder()
        .label("How rune starts Claude for this project. Applies to sessions you open from now on.")
        .xalign(0.0)
        .wrap(true)
        .build();
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    outer.append(&hint);

    // Model
    let model_dd = DropDown::from_strings(&PRESET_MODEL_LABELS);
    let model_sel = PRESET_MODEL_VALUES
        .iter()
        .position(|v| *v == cur.model.as_deref())
        .unwrap_or(0);
    model_dd.set_selected(model_sel as u32);
    model_dd.set_valign(gtk4::Align::Center);
    outer.append(&settings_row("Model", Some("Which Claude model to launch with"), &model_dd));

    // Permissions
    let perm_dd = DropDown::from_strings(&PRESET_PERM_LABELS);
    let perm_sel = PRESET_PERM_VALUES
        .iter()
        .position(|v| *v == cur.permission_mode.as_deref())
        .unwrap_or(0);
    perm_dd.set_selected(perm_sel as u32);
    perm_dd.set_valign(gtk4::Align::Center);
    outer.append(&settings_row(
        "Permissions",
        Some("How much it asks before acting"),
        &perm_dd,
    ));

    // Save on any change. Weak refs to the dropdowns avoid a widget↔closure cycle.
    let save = Rc::new({
        let st = state.clone();
        let path = path.to_string();
        let mw = model_dd.downgrade();
        let pw = perm_dd.downgrade();
        move || {
            let (Some(md), Some(pd)) = (mw.upgrade(), pw.upgrade()) else {
                return;
            };
            let preset = config::ProjectPreset {
                model: PRESET_MODEL_VALUES
                    .get(md.selected() as usize)
                    .and_then(|v| v.map(String::from)),
                permission_mode: PRESET_PERM_VALUES
                    .get(pd.selected() as usize)
                    .and_then(|v| v.map(String::from)),
            };
            st.borrow_mut().config.set_preset(&path, preset);
            save_state(&st);
        }
    });
    {
        let s = save.clone();
        model_dd.connect_selected_notify(move |_| s());
    }
    {
        let s = save.clone();
        perm_dd.connect_selected_notify(move |_| s());
    }

    let close = Button::with_label("Close");
    close.set_halign(gtk4::Align::End);
    close.set_margin_top(6);
    {
        let w = win.clone();
        close.connect_clicked(move |_| w.close());
    }
    outer.append(&close);

    {
        let w = win.clone();
        let key = EventControllerKey::new();
        key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, keyval, _code, _mods| {
            if keyval == gdk::Key::Escape {
                w.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        win.add_controller(key);
    }

    win.set_child(Some(&outer));
    win.present();
}

/// The settings dialog (MVP §3.7): terminal font + dark theme apply live; the
/// GPU renderer is saved for next launch; the exact-status hooks toggle is shown
/// but disabled (it's the opt-in P2.x installer that edits ~/.claude/settings.json).
fn open_settings(state: &State, parent: &ApplicationWindow) {
    let (font, dark, renderer) = {
        let s = state.borrow();
        (
            s.config.settings.terminal_font.clone().unwrap_or_default(),
            s.config.settings.prefer_dark.unwrap_or(DEFAULT_PREFER_DARK),
            s.config.settings.gsk_renderer.clone().unwrap_or_default(),
        )
    };

    // decorated(false): no server-side (mutter) frame. A decorated toplevel's
    // X-side frame teardown races GDK's EGL surface teardown under the GL
    // renderer we ship (GSK_RENDERER=ngl) and aborts the whole app on close;
    // a frameless window is destroyed by GDK in order, like the switcher. We
    // give it its own Close button + Esc instead of a titlebar.
    let win = Window::builder()
        .transient_for(parent)
        .modal(true)
        .decorated(false)
        .default_width(460)
        .resizable(false)
        .build();
    win.set_destroy_with_parent(true);
    win.add_css_class("rune-panel");

    let outer = gtk4::Box::new(Orientation::Vertical, 12);
    outer.set_margin_top(18);
    outer.set_margin_bottom(18);
    outer.set_margin_start(18);
    outer.set_margin_end(18);

    let title = Label::builder().label("Settings").xalign(0.0).build();
    title.add_css_class("title-4");
    outer.append(&title);

    outer.append(&section_label("Appearance"));

    // Terminal font — applies live (to open + future tabs) on Enter.
    let font_entry = Entry::builder()
        .text(&font)
        .placeholder_text("e.g. Monospace 12")
        .width_chars(16)
        .build();
    {
        let st = state.clone();
        font_entry.connect_activate(move |e| {
            let font = e.text().to_string();
            st.borrow_mut().config.settings.terminal_font =
                if font.trim().is_empty() { None } else { Some(font.clone()) };
            apply_font_all(&st, &font);
            save_state(&st);
        });
    }
    outer.append(&settings_row(
        "Terminal font",
        Some("Pango name; press Enter to apply"),
        &font_entry,
    ));

    // Dark theme — applies live.
    let dark_switch = Switch::new();
    dark_switch.set_active(dark);
    dark_switch.set_valign(gtk4::Align::Center);
    {
        let st = state.clone();
        dark_switch.connect_active_notify(move |sw| {
            let on = sw.is_active();
            st.borrow_mut().config.settings.prefer_dark = Some(on);
            apply_prefer_dark(Some(on));
            save_state(&st);
        });
    }
    outer.append(&settings_row(
        "Dark theme",
        Some("Affects system dialogs & menus — the cockpit itself is always dark"),
        &dark_switch,
    ));

    // GPU renderer — saved for next launch (can't swap a live GSK renderer).
    // Show the effective renderer (default "ngl"); a config value outside the
    // known presets (hand-edited) is surfaced as its own entry rather than
    // silently masquerading as a preset.
    let effective = if renderer.is_empty() { "ngl".to_string() } else { renderer };
    let mut options: Vec<String> = RENDERERS.iter().map(|s| s.to_string()).collect();
    let cur = match options.iter().position(|o| *o == effective) {
        Some(i) => i,
        None => {
            options.insert(0, effective);
            0
        }
    };
    let option_refs: Vec<&str> = options.iter().map(String::as_str).collect();
    let renderer_dd = DropDown::from_strings(&option_refs);
    renderer_dd.set_selected(cur as u32);
    renderer_dd.set_valign(gtk4::Align::Center);
    {
        let st = state.clone();
        let options = options.clone();
        renderer_dd.connect_selected_notify(move |dd| {
            if let Some(val) = options.get(dd.selected() as usize) {
                st.borrow_mut().config.settings.gsk_renderer = Some(val.clone());
                save_state(&st);
            }
        });
    }
    outer.append(&settings_row(
        "GPU renderer",
        Some("Applies on next launch · ngl avoids the Vulkan-on-NVIDIA crash"),
        &renderer_dd,
    ));

    outer.append(&section_label("Status"));

    // Opt-in hooks. Flipping this ON append-merges Stop / Notification /
    // UserPromptSubmit hooks into ~/.claude/settings.json (after a timestamped
    // backup, never overwriting the user's own hooks) so the needs-you queue can
    // tell "your turn" from "paused at a permission" exactly and show an activity
    // hint. OFF removes only rune's entries. The toggle itself is the consent;
    // the status line below reports exactly what happened.
    let installed = hooks::is_installed();
    let hooks_switch = Switch::new();
    hooks_switch.set_active(installed);
    hooks_switch.set_valign(gtk4::Align::Center);
    outer.append(&settings_row(
        "Exact awaiting-input",
        Some("Adds opt-in hooks to ~/.claude/settings.json (backed up first; reversible)"),
        &hooks_switch,
    ));

    let hooks_status = Label::builder().xalign(0.0).wrap(true).build();
    hooks_status.add_css_class("dim-label");
    hooks_status.add_css_class("caption");
    hooks_status.set_margin_start(2);
    if installed {
        hooks_status.set_text("On — exact awaiting-input & activity hints are live.");
    }
    outer.append(&hooks_status);

    {
        let st = state.clone();
        let status_lbl = hooks_status.clone();
        // Reverting the switch on error re-fires this handler; this flag makes the
        // programmatic revert a no-op so it can't run a second install/uninstall
        // or clobber the error message.
        let suppress = Rc::new(std::cell::Cell::new(false));
        hooks_switch.connect_active_notify(move |sw| {
            if suppress.get() {
                return;
            }
            let want_on = sw.is_active();
            status_lbl.remove_css_class("error-text");
            let outcome = if want_on {
                hooks::install().map(Some)
            } else {
                hooks::uninstall().map(|_| None)
            };
            match outcome {
                Ok(Some(backup)) => {
                    let where_to = backup
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let msg = if backup == hooks::settings_path() {
                        "On — created ~/.claude/settings.json with the hooks.".to_string()
                    } else {
                        format!("On — settings.json backed up to {where_to}.")
                    };
                    status_lbl.set_text(&msg);
                }
                Ok(None) => {
                    status_lbl.set_text("Off — rune's hooks removed; settings.json restored.");
                }
                Err(e) => {
                    suppress.set(true);
                    sw.set_active(!want_on); // revert without re-running the body
                    suppress.set(false);
                    status_lbl.add_css_class("error-text");
                    status_lbl.set_text(&format!(
                        "Couldn't {} hooks: {e}",
                        if want_on { "enable" } else { "disable" }
                    ));
                    return;
                }
            }
            // Reflect the new exactness immediately if the dashboard is showing.
            let on_home = st
                .borrow()
                .stack
                .visible_child_name()
                .map(|n| n == "home")
                .unwrap_or(false);
            if on_home {
                refresh_dashboard(&st);
            }
        });
    }

    // Frameless → give it an explicit close affordance + Esc.
    let close = Button::with_label("Close");
    close.set_halign(gtk4::Align::End);
    close.set_margin_top(6);
    {
        let w = win.clone();
        close.connect_clicked(move |_| w.close());
    }
    outer.append(&close);

    {
        let w = win.clone();
        let key = EventControllerKey::new();
        key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, keyval, _code, _mods| {
            if keyval == gdk::Key::Escape {
                w.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        win.add_controller(key);
    }

    win.set_child(Some(&outer));
    win.present();
}

/// The `GSK_RENDERER` presets offered in settings. When nothing is configured
/// the effective default is "ngl" (applied in main, before GTK starts).
const RENDERERS: [&str; 4] = ["ngl", "gl", "vulkan", "cairo"];

/// One settings row: a label (+ optional dim hint) on the left, a control right.
fn settings_row(label: &str, hint: Option<&str>, control: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    let row = gtk4::Box::new(Orientation::Horizontal, 12);
    let text = gtk4::Box::new(Orientation::Vertical, 1);
    text.set_hexpand(true);
    text.set_valign(gtk4::Align::Center);
    let lbl = Label::builder().label(label).xalign(0.0).build();
    text.append(&lbl);
    if let Some(h) = hint {
        let hl = Label::builder().label(h).xalign(0.0).wrap(true).build();
        hl.add_css_class("dim-label");
        hl.add_css_class("caption");
        text.append(&hl);
    }
    row.append(&text);
    row.append(control);
    row
}

fn section_label(text: &str) -> Label {
    let l = Label::builder().label(text).xalign(0.0).margin_top(4).build();
    l.add_css_class("heading");
    l
}

fn apply_prefer_dark(pref: Option<bool>) {
    if let Some(dark) = pref {
        if let Some(settings) = gtk4::Settings::default() {
            settings.set_gtk_application_prefer_dark_theme(dark);
        }
    }
}

fn apply_terminal_font(term: &Terminal, font: &str) {
    let font = font.trim();
    if font.is_empty() {
        term.set_font(None);
    } else {
        term.set_font(Some(&gtk4::pango::FontDescription::from_string(font)));
    }
}

fn apply_font_all(state: &State, font: &str) {
    let terms: Vec<Terminal> = state.borrow().tabs.iter().map(|t| t.terminal.clone()).collect();
    for term in &terms {
        apply_terminal_font(term, font);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Persistence
// ───────────────────────────────────────────────────────────────────────────

fn save_state(state: &State) {
    let config = {
        let mut s = state.borrow_mut();

        // Persist tabs in current tab-strip order (which may differ from
        // insertion order after the user drags tabs around).
        let mut open_tabs = Vec::new();
        let n = s.notebook.n_pages();
        for i in 0..n {
            let Some(child) = s.notebook.nth_page(Some(i)) else {
                continue;
            };
            let Ok(term) = child.downcast::<Terminal>() else {
                continue;
            };
            if let Some(info) = s.tabs.iter().find(|t| t.terminal == term) {
                open_tabs.push(OpenTab {
                    project_path: info.project_path.clone(),
                    session_id: info.session_id.clone(),
                    title: info.title.clone(),
                });
            }
        }
        s.config.open_tabs = open_tabs;
        s.config.selected_project = s.selected_project.clone();
        s.config.clone()
    };
    config.save();
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// App-wide CSS: the Mission Control finish for the whole shell (window chrome,
/// the project rail, the home dashboard). The per-project swatch colour classes
/// (`swatch-<n>`) are generated from [`palette::PALETTE`] so they can never
/// drift from `palette::color_index`.
fn install_css() {
    let mut css = String::from(BASE_CSS);
    for (i, hex) in palette::PALETTE.iter().enumerate() {
        css.push_str(&format!(".swatch-{i} {{ background-color: {hex}; }} "));
    }
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(&css);
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// The static Mission Control stylesheet (everything but the generated swatch
/// colours). Loaded at `PRIORITY_APPLICATION`, so these rules win over the
/// theme regardless of specificity. GTK CSS has no `text-transform`, so labels
/// that read uppercase are already uppercase in the source strings.
const BASE_CSS: &str = r##"
/* ===================== window chrome ===================== */
window { background-color: #070a0e; }

headerbar.rune-header {
  background-color: #0b0f15;
  background-image: linear-gradient(to bottom, #0b0f15, #070a0e);
  border-bottom: 1px solid #1b2531;
  box-shadow: none;
  color: #e8eef5;
}
headerbar.rune-header:backdrop { background-image: none; background-color: #090d12; }
headerbar.rune-header button.flat { color: #9fb0c0; border-radius: 5px; min-height: 28px; }
headerbar.rune-header button.flat:hover { color: #e8eef5; background-color: #131b25; }
headerbar.rune-header button.flat:active,
headerbar.rune-header button.flat:checked { color: #36d3fa; background-color: #131b25; }

/* brand */
.brand-word { font-family: monospace; font-weight: 700; font-size: 1.05em; letter-spacing: 1px; color: #e8eef5; }
.brand-sub { font-family: monospace; font-size: 0.62em; font-weight: 600; letter-spacing: 2px; color: #66788a; }

/* search field (a button dressed as a search box) */
.rune-search {
  background-color: #0f151d; background-image: none;
  border: 1px solid #1b2531; border-radius: 5px;
  padding: 3px 10px; box-shadow: none; min-height: 28px;
}
.rune-search:hover { border-color: #273545; background-color: #131b25; }
.search-icon { color: #66788a; }
.search-ph { color: #66788a; font-size: 0.92em; }
.kbd {
  font-family: monospace; font-size: 0.72em; color: #9fb0c0;
  border: 1px solid #273545; border-radius: 3px; padding: 0 6px;
  background-color: #070a0e;
}

.queue-badge { background-color: #ffb13b; color: #070a0e;
  border-radius: 9px; padding: 0 6px; font-size: 0.78em; font-weight: bold; }
.queue-badge-blocked { background-color: #ff6b6b; color: #ffffff; }

/* modal panels (Ctrl-K switcher, settings) */
.rune-panel { background-color: #0b0f15; border: 1px solid #273545; border-radius: 12px; }

/* ===================== project rail ===================== */
.rune-rail { background-color: #0b0f15; border-right: 1px solid #1b2531; }
.rune-rail scrolledwindow { background-color: transparent; }
.rune-rail viewport { background-color: transparent; }
.rail-list { background-color: transparent; }

.rail-head { padding: 12px 10px 8px 12px; }
.rail-head-label { font-family: monospace; font-size: 0.72em; font-weight: 700; letter-spacing: 1.5px; color: #66788a; }
.rail-tick { background-color: #36d3fa; border-radius: 2px; }
.rail-add { color: #66788a; min-height: 22px; min-width: 22px; padding: 2px; }
.rail-add:hover { color: #7fe4ff; background-color: #131b25; }

.proj-row { border-radius: 4px; }
.proj-row:hover { background-color: #0f151d; }
.proj-row:selected {
  background-image: linear-gradient(to right, rgba(54,211,250,0.10), rgba(54,211,250,0.01));
  box-shadow: inset 2px 0 0 0 #36d3fa;
  color: #e8eef5;
}
.proj-name { font-family: monospace; font-size: 0.92em; color: #9fb0c0; }
.proj-row:selected .proj-name { color: #e8eef5; }

.rail-badge { font-family: monospace; font-size: 0.7em; font-weight: 700; min-width: 18px; padding: 1px 5px; border-radius: 3px; }
.badge-idle { color: #44545f; background-color: #131b25; border: 1px solid #1b2531; }
.badge-work { color: #43e08a; background-color: rgba(67,224,138,0.08); border: 1px solid rgba(67,224,138,0.32); }
.badge-turn { color: #ffb13b; background-color: rgba(255,177,59,0.08); border: 1px solid rgba(255,177,59,0.34); }
.badge-blocked { color: #ff7b7b; background-color: rgba(255,107,107,0.10); border: 1px solid rgba(255,107,107,0.40); }

.rail-foot { padding: 9px 10px 9px 12px; border-top: 1px solid #1b2531; }
.rail-foot-label { font-family: monospace; font-size: 0.74em; color: #66788a; }
.rail-live { background-color: #43e08a; border-radius: 3px; animation: rune-pulse 2.4s ease-in-out infinite; }
@keyframes rune-pulse { 0% { opacity: 1; } 50% { opacity: 0.35; } 100% { opacity: 1; } }

paned > separator { background-color: #1b2531; }

/* per-project identity swatch (colour classes generated in install_css) */
.swatch { border-radius: 2px; }

/* ===================== home dashboard ===================== */
.rune-dash { background-color: #0e1217;
  background-image: linear-gradient(rgba(110,200,225,0.05) 1px, transparent 1px),
    linear-gradient(90deg, rgba(110,200,225,0.05) 1px, transparent 1px);
  background-size: 34px 34px; padding: 22px; }
.dash-greeting { font-size: 1.7em; font-weight: 800; }
.dash-section { font-size: 0.76em; font-weight: 700; color: #5fd0e0; margin-top: 6px; }
.tnum { font-family: monospace; }
.dash-card { background-color: rgba(18,26,34,0.66);
  border: 1px solid rgba(120,200,225,0.16); border-radius: 6px; padding: 10px 13px; }
.dash-card:hover { border-color: rgba(95,208,224,0.6); background-color: rgba(24,34,44,0.9); }
.dash-card-name { font-weight: 700; }
.dash-card-cost { color: #9fb0c0; font-size: 0.92em; }
.dash-list { border: 1px solid rgba(120,200,225,0.14); border-radius: 8px;
  background-color: rgba(14,20,26,0.55); }
.dash-list > row:hover { background-color: rgba(95,208,224,0.09); }
.dash-pj { color: #5fd0e0; }
.dash-midot { color: #44545f; }
.branch-chip { font-family: monospace; font-size: 0.8em; color: #9fb0c0; }
.chip { border: 1px solid #273545; border-radius: 3px; padding: 1px 8px;
  background-color: rgba(15,21,29,0.85); }
.chip label { font-family: monospace; font-size: 0.8em; color: #9fb0c0; }
.chip-dot { border-radius: 3px; background-color: #66788a; min-width: 6px; min-height: 6px; }
.chip-opus { border-color: rgba(54,211,250,0.42); background-color: rgba(54,211,250,0.09); }
.chip-opus label { color: #5fd0e0; }  .chip-opus .chip-dot { background-color: #36d3fa; }
.chip-sonnet { border-color: rgba(142,162,255,0.42); background-color: rgba(142,162,255,0.09); }
.chip-sonnet label { color: #aeb9ff; }  .chip-sonnet .chip-dot { background-color: #8ea2ff; }
.chip-haiku { border-color: rgba(67,224,138,0.42); background-color: rgba(67,224,138,0.09); }
.chip-haiku label { color: #6fe0a8; }  .chip-haiku .chip-dot { background-color: #43e08a; }
.chip-fable { border-color: rgba(255,177,59,0.42); background-color: rgba(255,177,59,0.09); }
.chip-fable label { color: #ffc46b; }  .chip-fable .chip-dot { background-color: #ffb13b; }
.ctx-lbl { font-family: monospace; font-size: 0.68em; color: #5e6b78; }
.ctx-val { font-family: monospace; font-size: 0.72em; color: #9fb0c0; }
.ctx-track { border-radius: 3px; background-color: #070a0e; box-shadow: inset 0 0 0 1px #1b2531; }
.ctx-fill { border-radius: 3px; background-image: linear-gradient(to right, #0e8db0, #36d3fa); }
.ctx-fill.lvl-mid { background-image: linear-gradient(to right, #b86a1a, #ffb13b); }
.ctx-fill.lvl-hi { background-image: linear-gradient(to right, #b8323e, #ff5e6c); }
.scost-amt { font-family: monospace; color: #e8eef5; }
.scost-when { font-family: monospace; font-size: 0.78em; color: #5e6b78; }

/* greeting glance stat-pills */
.glance-pill { padding: 3px 10px 3px 8px; border-radius: 3px;
  border: 1px solid #1b2531; background-color: rgba(15,21,29,0.7); }
.glance-pill-label { font-family: monospace; font-size: 0.84em; color: #c7d2dd; }
.glance-dot { border-radius: 4px; }
.glance-dot-block { background-color: #ff6b6b; }
.glance-dot-turn { background-color: #ffb13b; }
.glance-dot-work { background-color: #43e08a; }

/* needs-you command board */
.command-board { background-color: #0b0f15;
  background-image: linear-gradient(to bottom, rgba(54,211,250,0.05), transparent 70px);
  border: 1px solid #273545; border-radius: 6px; }
.command-banner { padding: 13px 18px; border-bottom: 1px solid #1b2531;
  background-image: linear-gradient(to bottom, #0f151d, transparent); }
.command-glyph-chip { min-width: 30px; min-height: 30px; border-radius: 4px;
  background-color: rgba(54,211,250,0.07); border: 1px solid rgba(54,211,250,0.32);
  box-shadow: inset 0 0 14px -4px rgba(54,211,250,0.16); }
.command-glyph { color: #7fe4ff; }
.command-title { font-family: monospace; font-weight: 700; font-size: 0.82em; letter-spacing: 1.5px; color: #e8eef5; }
.command-sub { font-size: 0.92em; color: #66788a; }

.command-row { background-color: transparent; background-image: none; border: none;
  box-shadow: none; border-bottom: 1px solid #1b2531; border-radius: 0;
  padding: 11px 18px 11px 0; min-height: 0; }
.command-row:hover { background-color: #0f151d; }
.command-row:focus-visible { outline: none; box-shadow: inset 0 0 0 1px rgba(54,211,250,0.45); }
.command-row:disabled { opacity: 0.55; }
.command-row:disabled:hover { background-color: transparent; }
.dash-card:focus-visible { outline: none; box-shadow: inset 0 0 0 1px rgba(54,211,250,0.45); }

.cmd-stripe { border-radius: 0; }
.row-block .cmd-stripe { background-color: #ff6b6b; }
.row-turn .cmd-stripe { background-color: #ffb13b; }
.row-work .cmd-stripe { background-color: #43e08a; }
.row-done .cmd-stripe { background-color: #8ea2ff; }

.cmd-pill { padding: 5px 10px 5px 8px; border-radius: 2px; }
.cmd-pill-label { font-family: monospace; font-size: 0.74em; font-weight: 700; letter-spacing: 0.6px; }
.cmd-pd { border-radius: 4px; }
.pill-block { background-color: rgba(255,107,107,0.10); box-shadow: inset 0 0 0 1px rgba(255,107,107,0.40); }
.pill-block .cmd-pill-label { color: #ff7b7b; }
/* The blocked dot pulses — a permission stall is the one state that wants your eye now. */
.pill-block .cmd-pd { background-color: #ff6b6b; animation: rune-pulse 1.5s ease-in-out infinite; }
.pill-turn { background-color: rgba(255,177,59,0.09); box-shadow: inset 0 0 0 1px rgba(255,177,59,0.34); }
.pill-turn .cmd-pill-label { color: #ffb13b; }  .pill-turn .cmd-pd { background-color: #ffb13b; }
.pill-work { background-color: rgba(67,224,138,0.09); box-shadow: inset 0 0 0 1px rgba(67,224,138,0.32); }
.pill-work .cmd-pill-label { color: #43e08a; }  .pill-work .cmd-pd { background-color: #43e08a; }
.pill-done { background-color: rgba(142,162,255,0.09); box-shadow: inset 0 0 0 1px rgba(142,162,255,0.32); }
.pill-done .cmd-pill-label { color: #8ea2ff; }  .pill-done .cmd-pd { background-color: #8ea2ff; }

.cmd-title { color: #e8eef5; }
.cmd-hint { color: #6f8296; font-style: italic; }
.error-text { color: #ff7b7b; }
.cmd-cost { font-family: monospace; color: #c7d2dd; font-size: 0.95em; }
.cmd-action { font-family: monospace; font-size: 0.82em; font-weight: 600; letter-spacing: 0.3px;
  padding: 6px 13px; border-radius: 4px; border: 1px solid #273545; background-color: #0f151d; color: #9fb0c0; }
.cmd-action-primary { border-color: rgba(54,211,250,0.42); background-color: rgba(54,211,250,0.10); color: #7fe4ff; }
.command-row:hover .cmd-action { border-color: #3a4a5a; color: #e8eef5; }
.command-row:hover .cmd-action-primary { border-color: #36d3fa; background-color: rgba(54,211,250,0.18); color: #ffffff; }

/* dashboard footer */
.dash-foot { border-top: 1px solid #1b2531; padding-top: 13px; }
.foot-wm { font-family: monospace; font-weight: 700; color: #36d3fa; }
.foot-text { font-family: monospace; font-size: 0.85em; color: #5e6b78; }
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// The dropdown values the launch-preset UI offers must be exactly the
    /// allowlist `terminal.rs` will actually pass to `claude`. `terminal.rs` is
    /// the security boundary; this keeps the two hand-written lists from drifting
    /// (a UI value missing there would be silently dropped at launch).
    #[test]
    fn ui_preset_values_match_the_launch_allowlist() {
        let ui_models: Vec<&str> = PRESET_MODEL_VALUES.iter().filter_map(|v| *v).collect();
        assert_eq!(ui_models, crate::terminal::MODEL_ALIASES.to_vec());
        let ui_perms: Vec<&str> = PRESET_PERM_VALUES.iter().filter_map(|v| *v).collect();
        assert_eq!(ui_perms, crate::terminal::PERMISSION_MODES.to_vec());
    }

    /// The label and value arrays must stay index-aligned (the dropdown maps a
    /// selected row index straight into the values array).
    #[test]
    fn preset_labels_and_values_are_aligned() {
        assert_eq!(PRESET_MODEL_LABELS.len(), PRESET_MODEL_VALUES.len());
        assert_eq!(PRESET_PERM_LABELS.len(), PRESET_PERM_VALUES.len());
    }
}
