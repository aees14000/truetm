//! dvtr - A truecolor-enabled terminal multiplexer inspired by dvtm

mod config;
mod copy_mode;
mod layout;
mod mouse;
mod pane;
mod reflow;
mod render;
mod tag;

use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind, MouseButton,
        KeyboardEnhancementFlags, PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        EnableMouseCapture, DisableMouseCapture,
    },
    execute, queue,
    style::ResetColor,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use layout::LayoutManager;
use pane::{Pane, PaneId, PaneManager, PtyMessage, Rect};
use render::{Compositor, ScreenBuffer};
use copy_mode::CopyModeState;
use tag::TagSet;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::path::Path;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    env_logger::init();

    let result = run();

    // Cleanup - position cursor at bottom before leaving alternate screen to avoid blank line
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    let _ = terminal::disable_raw_mode();
    // Reset cursor style to terminal default in case a pane left it as a bar/underline.
    let _ = write!(io::stdout(), "\x1b[0 q");
    let _ = execute!(io::stdout(), ResetColor, Show, LeaveAlternateScreen);

    result
}

/// Pending commands that need a second keypress
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingCommand {
    ViewTag,    // 'v' - waiting for tag number
    SetTag,     // 't' - waiting for tag number
    ToggleTag,  // 'T' - waiting for tag number
}

/// Mouse selection state
#[derive(Debug, Clone)]
struct MouseSelection {
    pane_id: PaneId,
    // Buffer coordinates (relative to pane content area)
    buf_start_x: u16,
    buf_start_y: u16,
    buf_end_x: u16,
    buf_end_y: u16,
}

/// Per-tag state (focus and broadcast mode)
#[derive(Debug, Clone, Default)]
struct TagState {
    focused_pane_id: Option<PaneId>,
    broadcast_mode: bool,
}

/// Application state
struct App {
    panes: PaneManager,
    buffers: HashMap<PaneId, ScreenBuffer>,
    layout: LayoutManager,
    compositor: Compositor,
    pty_tx: Sender<PtyMessage>,
    pty_rx: Receiver<PtyMessage>,
    shell: String,
    env_vars: Vec<(String, String)>,
    width: u16,
    height: u16,
    prefix_mode: bool,
    pending_command: Option<PendingCommand>,
    needs_redraw: bool,
    running: bool,
    // Tagging
    current_view: TagSet,
    tag_count: u8,
    // Tag history stack (unique - each tag appears once, most recent at end)
    tag_history: Vec<u8>,
    // Broadcast mode - send input to all visible panes
    broadcast_mode: bool,
    // Per-tag state (focus and broadcast mode)
    tag_states: HashMap<u8, TagState>,
    // Copy mode - vim-style scrollback navigation and selection
    copy_mode: Option<CopyModeState>,
    // Zoom mode - focused pane is fullscreen
    zoomed_pane: Option<PaneId>,
    // Mouse selection
    mouse_selection: Option<MouseSelection>,
    // Per-tag status bar entries, derived from the panes carrying each tag
    tag_info: HashMap<u8, TagInfo>,
}

impl App {
    fn new(width: u16, height: u16) -> Self {
        let (pty_tx, pty_rx) = mpsc::channel();

        // Collect environment variables
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut env_vars = vec![
            ("TERM".to_string(), std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string())),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ];

        for var in ["HOME", "USER", "SHELL", "PATH", "LANG", "LC_ALL", "LC_CTYPE",
                    "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
                    "EDITOR", "VISUAL", "PAGER"] {
            if let Ok(val) = std::env::var(var) {
                env_vars.push((var.to_string(), val));
            }
        }

        Self {
            panes: PaneManager::new(),
            buffers: HashMap::new(),
            layout: LayoutManager::new(),
            compositor: Compositor::new(width, height - 1), // -1 for status bar
            pty_tx,
            pty_rx,
            shell,
            env_vars,
            width,
            height,
            prefix_mode: false,
            pending_command: None,
            needs_redraw: true,
            running: true,
            current_view: TagSet::single(0), // Start on tag 1 (index 0)
            tag_count: 9,
            tag_history: vec![0], // Start with tag 0 in history
            broadcast_mode: false,
            tag_states: HashMap::new(),
            copy_mode: None,
            zoomed_pane: None,
            mouse_selection: None,
            tag_info: HashMap::new(),
        }
    }

    /// Re-derive what the status bar shows for each tag. Returns true if
    /// anything changed, so the caller can redraw.
    ///
    /// Called on a timer rather than per frame: each tag costs one readlink of
    /// /proc/<pid>/cwd, and the label has to follow the shell as it cds.
    fn refresh_tag_info(&mut self) -> bool {
        let mut fresh = HashMap::new();
        for tag in 0..self.tag_count {
            if let Some(cwd) = self.panes.master_with_tag(tag).and_then(|p| p.get_cwd()) {
                fresh.insert(tag, TagInfo {
                    label: dir_label(&cwd),
                    panes: self.panes.count_with_tag(tag),
                });
            }
        }
        if fresh == self.tag_info {
            return false;
        }
        self.tag_info = fresh;
        true
    }

    /// Switch to a tag, updating history (unique stack - moves tag to top if already present)
    fn switch_to_tag(&mut self, tag: u8) {
        // Remove tag from history if present (to avoid duplicates)
        self.tag_history.retain(|&t| t != tag);
        // Push to top
        self.tag_history.push(tag);
        // Update view
        self.current_view = TagSet::single(tag);
        // Force full redraw since visible panes changed
        self.compositor.invalidate();
    }

    /// Go to previous tag in history (pop current, switch to new top)
    fn go_to_previous_tag(&mut self) {
        if self.tag_history.len() > 1 {
            // Pop current tag
            self.tag_history.pop();
            // Switch to new top
            if let Some(&tag) = self.tag_history.last() {
                self.current_view = TagSet::single(tag);
            }
            // Force full redraw since visible panes changed
            self.compositor.invalidate();
        }
    }

    /// Save current focus and broadcast state to all tags in current view
    fn save_tag_state(&mut self) {
        // Don't save state when viewing ALL tags
        if self.current_view == TagSet::ALL {
            return;
        }

        if let Some(pane) = self.panes.focused() {
            let pane_id = pane.id;
            for tag in 0..self.tag_count {
                if self.current_view.contains(tag) {
                    let state = self.tag_states.entry(tag).or_default();
                    state.focused_pane_id = Some(pane_id);
                    state.broadcast_mode = self.broadcast_mode;
                }
            }
        }
    }

    /// Restore focus and broadcast state from a tag
    fn restore_tag_state(&mut self, tag: u8) {
        if let Some(state) = self.tag_states.get(&tag).cloned() {
            self.broadcast_mode = state.broadcast_mode;
            if let Some(pane_id) = state.focused_pane_id {
                // Only focus if pane exists and is visible in current view
                let visible = self.panes.visible_in_view(self.current_view);
                if visible.contains(&pane_id) {
                    self.panes.focus_by_id(pane_id);
                }
            }
        }
    }

    /// Create a new pane with current view's tags, inheriting cwd from focused pane
    fn create_pane(&mut self) -> Result<PaneId> {
        // Get cwd from focused pane, or use current directory for initial pane
        let cwd = self.panes.focused()
            .and_then(|p| p.get_cwd())
            .or_else(|| std::env::current_dir().ok());
        self.create_pane_with_tags(self.current_view, cwd)
    }

    /// Create a new pane with specific tags and optional working directory
    fn create_pane_with_tags(&mut self, tags: TagSet, cwd: Option<std::path::PathBuf>) -> Result<PaneId> {
        let id = self.panes.next_id();

        // Initial rect - will be updated by layout
        let content_height = self.height.saturating_sub(1); // Reserve 1 row for status bar
        let rect = Rect::new(0, 0, self.width, content_height);

        // Buffer height is rect.height - 1 to reserve header row
        let buffer_height = rect.height.saturating_sub(1);
        let pane = Pane::new_with_size(id, rect, tags, &self.shell, &self.env_vars, cwd, self.pty_tx.clone(), rect.width, buffer_height)?;
        // add() inserts at front (master) and focuses
        self.panes.add(pane);
        self.buffers.insert(id, ScreenBuffer::new(rect.width, buffer_height));

        self.apply_layout();
        self.save_tag_state();
        self.needs_redraw = true;

        Ok(id)
    }

    /// Close the focused pane
    fn close_focused_pane(&mut self) {
        if let Some(pane) = self.panes.focused() {
            let id = pane.id;

            // Exit zoom mode if closing zoomed pane
            if self.zoomed_pane == Some(id) {
                self.zoomed_pane = None;
            }

            self.panes.remove(id);
            self.buffers.remove(&id);

            // If current tag is now empty, go to previous tag in history
            let went_to_previous = if self.panes.visible_in_view(self.current_view).is_empty() {
                self.go_to_previous_tag();
                true
            } else {
                false
            };

            self.apply_layout();

            // Restore tag state if we switched tags
            if went_to_previous {
                if let Some(&tag) = self.tag_history.last() {
                    self.restore_tag_state(tag);
                }
            }

            self.needs_redraw = true;
        }
    }

    /// Apply the current layout to visible panes
    fn apply_layout(&mut self) {
        // Only layout panes visible in current view
        let pane_ids = self.panes.visible_in_view(self.current_view);
        // Content area excludes status bar (1 row at bottom)
        let content_height = self.height.saturating_sub(1);
        let area = Rect::new(0, 0, self.width, content_height);

        // In zoom mode, only show the zoomed pane
        if let Some(zoomed_id) = self.zoomed_pane {
            // Check if zoomed pane is still visible in current view
            if pane_ids.contains(&zoomed_id) {
                // Give zoomed pane the full area
                let buffer_height = area.height.saturating_sub(1);
                if let Some(pane) = self.panes.get_mut(zoomed_id) {
                    pane.set_rect_with_size(area, area.width, buffer_height);
                }
                if let Some(buffer) = self.buffers.get_mut(&zoomed_id) {
                    buffer.resize(area.width, buffer_height);
                }
                // Hide all other panes
                for &pane_id in &pane_ids {
                    if pane_id != zoomed_id {
                        if let Some(pane) = self.panes.get_mut(pane_id) {
                            pane.rect = Rect::new(0, 0, 0, 0);
                        }
                    }
                }
            } else {
                // Zoomed pane no longer visible, exit zoom mode
                self.zoomed_pane = None;
                self.apply_layout(); // Re-apply without zoom
                return;
            }
        } else {
            // Normal layout
            let positions = self.layout.arrange(&pane_ids, area);

            // Track which panes are in the layout
            let layout_pane_ids: Vec<_> = positions.iter().map(|(id, _)| *id).collect();

            for (pane_id, rect) in positions {
                // Buffer/PTY height is rect.height - 1 to reserve header row
                let buffer_height = rect.height.saturating_sub(1);
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.set_rect_with_size(rect, rect.width, buffer_height);
                }
                if let Some(buffer) = self.buffers.get_mut(&pane_id) {
                    buffer.resize(rect.width, buffer_height);
                }
            }

            // Hide panes not in layout (e.g., in monocle mode)
            for &pane_id in &pane_ids {
                if !layout_pane_ids.contains(&pane_id) {
                    if let Some(pane) = self.panes.get_mut(pane_id) {
                        pane.rect = Rect::new(0, 0, 0, 0);
                    }
                }
            }
        }

        // Ensure focus is on a visible pane
        self.panes.ensure_focus_in_view(self.current_view);

        // Force full redraw when layout changes
        self.compositor.invalidate();
    }

    /// Handle resize
    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.compositor.resize(width, height.saturating_sub(1)); // -1 for status bar
        self.apply_layout();
        self.needs_redraw = true;
    }

    /// Process PTY output, returns true if any data was processed
    fn process_pty_messages(&mut self) -> bool {
        let mut had_data = false;

        while let Ok(msg) = self.pty_rx.try_recv() {
            match msg {
                PtyMessage::Data { pane_id, data } => {
                    if let Some(buffer) = self.buffers.get_mut(&pane_id) {
                        buffer.process(&data);
                        self.needs_redraw = true;
                        had_data = true;

                        // Send any terminal responses back to the PTY
                        let responses = buffer.drain_responses();
                        if !responses.is_empty() {
                            if let Some(pane) = self.panes.get_mut(pane_id) {
                                for response in responses {
                                    let _ = pane.write(&response);
                                }
                            }
                        }
                    }
                }
                PtyMessage::Exit { pane_id } => {
                    self.panes.mark_exited(pane_id);
                }
            }
        }

        self.panes.poll_exited();

        // Clean up exited panes
        let had_exited = self.panes.all().iter().any(|p| p.exited);
        if had_exited {
            let exited_ids: Vec<_> = self.panes.all().iter().filter(|p| p.exited).map(|p| p.id).collect();
            for id in exited_ids {
                self.buffers.remove(&id);
            }
            self.panes.remove_exited();

            // If current tag is now empty, go to previous tag in history
            let went_to_previous = if self.panes.visible_in_view(self.current_view).is_empty() {
                self.go_to_previous_tag();
                true
            } else {
                false
            };

            self.apply_layout();

            // Restore tag state if we switched tags
            if went_to_previous {
                if let Some(&tag) = self.tag_history.last() {
                    self.restore_tag_state(tag);
                }
            }

            self.needs_redraw = true;
        }

        had_data
    }

    /// Handle keyboard input.
    /// Per-pane failures (a PTY write to a dying shell, a pane that could
    /// not be created) are logged, never propagated: one bad pane must not
    /// take down the whole session.
    fn handle_key(&mut self, key: KeyEvent) {
        // Check for prefix key
        if !self.prefix_mode
            && self.pending_command.is_none()
            && key.modifiers.contains(config::PREFIX_MODIFIERS)
            && key.code == config::PREFIX_KEY
        {
            self.prefix_mode = true;
            return;
        }

        // Handle pending command (waiting for a number after v/t/T)
        if let Some(pending) = self.pending_command {
            self.pending_command = None;

            if let KeyCode::Char(c) = key.code {
                if let Some(num) = c.to_digit(10) {
                    match pending {
                        PendingCommand::ViewTag => {
                            if num >= 1 && num <= 9 {
                                let tag = (num - 1) as u8;
                                self.switch_to_tag(tag);
                                // Auto-create pane if tag is empty
                                if self.panes.visible_in_view(self.current_view).is_empty() {
                                    if let Err(e) = self.create_pane() {
                                        log::error!("failed to create pane: {:#}", e);
                                    }
                                }
                                self.apply_layout();
                                // Restore saved focus and broadcast state for this tag
                                self.restore_tag_state(tag);
                                self.needs_redraw = true;
                            } else if num == 0 {
                                // View all tags (doesn't affect history)
                                self.current_view = TagSet::ALL;
                                self.apply_layout();
                                self.needs_redraw = true;
                            }
                        }
                        PendingCommand::SetTag => {
                            if num >= 1 && num <= 9 {
                                let tag = (num - 1) as u8;
                                if let Some(pane) = self.panes.focused_mut() {
                                    pane.tags = TagSet::single(tag);
                                }
                                self.apply_layout();
                                self.needs_redraw = true;
                            }
                        }
                        PendingCommand::ToggleTag => {
                            if num >= 1 && num <= 9 {
                                let tag = (num - 1) as u8;
                                if let Some(pane) = self.panes.focused_mut() {
                                    pane.tags.toggle(tag);
                                    // Ensure pane has at least one tag
                                    if pane.tags.is_empty() {
                                        pane.tags = TagSet::single(tag);
                                    }
                                }
                                self.apply_layout();
                                self.needs_redraw = true;
                            }
                        }
                    }
                }
            }
            return;
        }

        if self.prefix_mode {
            self.prefix_mode = false;

            // Check for window focus (1-9 focuses visible window N)
            if let KeyCode::Char(c) = key.code {
                if let Some(num) = c.to_digit(10) {
                    if num >= 1 && num <= 9 {
                        // Focus visible window N (1-indexed)
                        let visible = self.panes.visible_in_view(self.current_view);
                        let idx = (num - 1) as usize;
                        if idx < visible.len() {
                            self.panes.focus_by_id(visible[idx]);
                            self.save_tag_state();
                            self.needs_redraw = true;
                        }
                        return;
                    }
                }
            }

            match key.code {
                k if k == config::KEY_QUIT => {
                    self.running = false;
                }
                k if k == config::KEY_NEW_WINDOW => {
                    if let Err(e) = self.create_pane() {
                        log::error!("failed to create pane: {:#}", e);
                    }
                }
                k if k == config::KEY_CLOSE_WINDOW => {
                    self.close_focused_pane();
                    if self.panes.is_empty() {
                        self.running = false;
                    }
                }
                k if k == config::KEY_FOCUS_LEFT => {
                    self.panes.focus_direction(self.current_view, -1, 0);
                    self.save_tag_state();
                    self.needs_redraw = true;
                }
                k if k == config::KEY_FOCUS_DOWN => {
                    self.panes.focus_direction(self.current_view, 0, 1);
                    self.save_tag_state();
                    self.needs_redraw = true;
                }
                k if k == config::KEY_FOCUS_UP => {
                    self.panes.focus_direction(self.current_view, 0, -1);
                    self.save_tag_state();
                    self.needs_redraw = true;
                }
                k if k == config::KEY_FOCUS_RIGHT => {
                    self.panes.focus_direction(self.current_view, 1, 0);
                    self.save_tag_state();
                    self.needs_redraw = true;
                }
                k if k == config::KEY_MASTER_SHRINK => {
                    self.layout.adjust_master(-config::MASTER_ADJUST_STEP);
                    self.apply_layout();
                    self.needs_redraw = true;
                }
                k if k == config::KEY_MASTER_GROW => {
                    self.layout.adjust_master(config::MASTER_ADJUST_STEP);
                    self.apply_layout();
                    self.needs_redraw = true;
                }
                k if k == config::PREFIX_KEY => {
                    // Send literal prefix key (Ctrl+letter = letter - 'a' + 1)
                    if let KeyCode::Char(c) = config::PREFIX_KEY {
                        let ctrl_byte = (c.to_ascii_lowercase() as u8) - b'a' + 1;
                        if let Some(pane) = self.panes.focused_mut() {
                            write_to_pane(pane, &[ctrl_byte]);
                        }
                    }
                }
                k if k == config::KEY_SWAP_MASTER => {
                    self.panes.swap_with_master(self.current_view);
                    self.apply_layout();
                    self.save_tag_state();
                    self.needs_redraw = true;
                }
                k if k == config::KEY_VIEW_TAG => {
                    self.pending_command = Some(PendingCommand::ViewTag);
                }
                k if k == config::KEY_SET_TAG => {
                    self.pending_command = Some(PendingCommand::SetTag);
                }
                k if k == config::KEY_TOGGLE_TAG => {
                    self.pending_command = Some(PendingCommand::ToggleTag);
                }
                k if k == config::KEY_TOGGLE_BROADCAST => {
                    self.broadcast_mode = !self.broadcast_mode;
                    self.save_tag_state();
                    self.needs_redraw = true;
                }
                k if k == config::KEY_ENTER_COPY => {
                    // Enter copy mode
                    if let Some(pane) = self.panes.focused() {
                        if let Some(buffer) = self.buffers.get(&pane.id) {
                            self.copy_mode = Some(CopyModeState::new(
                                buffer.width(),
                                buffer.height(),
                                buffer.scrollback_len(),
                            ));
                            self.compositor.invalidate();
                            self.needs_redraw = true;
                        }
                    }
                }
                // Enter copy mode with initial motion
                KeyCode::PageUp | KeyCode::PageDown | KeyCode::Up | KeyCode::Down => {
                    if let Some(pane) = self.panes.focused() {
                        if let Some(buffer) = self.buffers.get(&pane.id) {
                            let mut copy_state = CopyModeState::new(
                                buffer.width(),
                                buffer.height(),
                                buffer.scrollback_len(),
                            );
                            // Perform initial motion
                            match key.code {
                                KeyCode::PageUp => copy_state.page_up(),
                                KeyCode::PageDown => copy_state.page_down(),
                                KeyCode::Up => copy_state.move_up(),
                                KeyCode::Down => copy_state.move_down(),
                                _ => {}
                            }
                            self.copy_mode = Some(copy_state);
                            self.compositor.invalidate();
                            self.needs_redraw = true;
                        }
                    }
                }
                k if k == config::KEY_ZOOM => {
                    if let Some(focused) = self.panes.focused() {
                        let focused_id = focused.id;
                        if self.zoomed_pane == Some(focused_id) {
                            // Already zoomed on this pane, unzoom
                            self.zoomed_pane = None;
                        } else {
                            // Zoom on focused pane
                            self.zoomed_pane = Some(focused_id);
                        }
                        self.apply_layout();
                        self.needs_redraw = true;
                    }
                }
                _ => {}
            }
            return;
        }

        // Handle copy mode
        if self.copy_mode.is_some() {
            let mut exit_copy_mode = false;
            let mut yank_selection = false;
            let mut do_first_non_blank = false;
            let mut do_line_end = false;
            let mut do_word_motion: Option<(bool, bool, bool)> = None; // (forward, end, big_word)
            let mut do_search_next = false;
            let mut do_search_prev = false;
            let mut do_find_char: Option<(bool, bool)> = None; // (forward, inclusive)
            let mut do_repeat_find = false;
            let mut do_repeat_find_reverse = false;

            // Check if we're in search input mode or pending find char
            let in_search_input = self.copy_mode.as_ref()
                .map(|c| c.search_mode != copy_mode::SearchMode::None)
                .unwrap_or(false);
            let pending_find = self.copy_mode.as_ref()
                .map(|c| c.pending_find.is_some())
                .unwrap_or(false);

            // Handle search input mode
            if in_search_input {
                if let Some(ref mut copy_state) = self.copy_mode {
                    match key.code {
                        KeyCode::Esc => {
                            copy_state.cancel_search();
                        }
                        KeyCode::Enter => {
                            // Execute search handled below
                        }
                        KeyCode::Backspace => {
                            copy_state.search_pop_char();
                        }
                        KeyCode::Char(c) => {
                            copy_state.search_push_char(c);
                        }
                        _ => {}
                    }
                }
                // Execute search on Enter
                if key.code == KeyCode::Enter {
                    if let Some(pane) = self.panes.focused() {
                        if let Some(buffer) = self.buffers.get(&pane.id) {
                            let get_line = |y: i32| get_line_content_static(buffer, y);
                            if let Some(ref mut copy_state) = self.copy_mode {
                                copy_state.execute_search(get_line);
                            }
                        }
                    }
                }
                self.compositor.invalidate();
                self.needs_redraw = true;
                return;
            }

            // Handle pending find char (f/F/t/T waiting for char)
            if pending_find {
                if let KeyCode::Char(c) = key.code {
                    if let Some(pane) = self.panes.focused() {
                        if let Some(buffer) = self.buffers.get(&pane.id) {
                            if let Some(ref copy_state) = self.copy_mode {
                                let line = self.get_line_content(buffer, copy_state.cursor.y);
                                if let Some(ref mut cs) = self.copy_mode {
                                    cs.do_find_char(c, &line);
                                    cs.reset_count();
                                }
                            }
                        }
                    }
                } else if key.code == KeyCode::Esc {
                    if let Some(ref mut copy_state) = self.copy_mode {
                        copy_state.pending_find = None;
                    }
                }
                self.compositor.invalidate();
                self.needs_redraw = true;
                return;
            }

            // Check if we're in pending text object mode (i/a waiting for object type)
            let pending_text_object = self.copy_mode.as_ref()
                .map(|c| c.pending_text_object.is_some())
                .unwrap_or(false);

            // Handle pending text object (i/a waiting for object type like w, ", (, etc.)
            if pending_text_object {
                if let KeyCode::Char(c) = key.code {
                    // Valid text object types: w, W, ", ', `, (, ), b, [, ], {, }, B, <, >
                    match c {
                        'w' | 'W' | '"' | '\'' | '`' | '(' | ')' | 'b' | '[' | ']' | '{' | '}' | 'B' | '<' | '>' => {
                            if let Some(pane) = self.panes.focused() {
                                if let Some(buffer) = self.buffers.get(&pane.id) {
                                    if let Some(ref copy_state) = self.copy_mode {
                                        let line = self.get_line_content(buffer, copy_state.cursor.y);
                                        if let Some(ref mut cs) = self.copy_mode {
                                            cs.select_text_object(c, &line);
                                            cs.reset_count();
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            // Invalid object type, cancel
                            if let Some(ref mut cs) = self.copy_mode {
                                cs.pending_text_object = None;
                                cs.reset_count();
                            }
                        }
                    }
                } else if key.code == KeyCode::Esc {
                    if let Some(ref mut copy_state) = self.copy_mode {
                        copy_state.pending_text_object = None;
                    }
                }
                self.compositor.invalidate();
                self.needs_redraw = true;
                return;
            }

            if let Some(ref mut copy_state) = self.copy_mode {
                let count = copy_state.get_count();

                match key.code {
                    // Exit: q always exits, Esc exits visual first then copy mode
                    k if k == config::COPY_EXIT_1 => {
                        exit_copy_mode = true;
                    }
                    k if k == config::COPY_EXIT_2 => {
                        // Escape: if in visual mode, exit visual but stay in copy mode
                        if copy_state.visual_mode != copy_mode::VisualMode::None {
                            copy_state.visual_mode = copy_mode::VisualMode::None;
                            copy_state.selection = None;
                        } else {
                            exit_copy_mode = true;
                        }
                    }

                    // Numeric prefix (1-9 start, 0 continues if count started)
                    KeyCode::Char(c @ '1'..='9') => {
                        copy_state.push_count_digit(c.to_digit(10).unwrap());
                    }
                    KeyCode::Char('0') if copy_state.count.is_some() => {
                        copy_state.push_count_digit(0);
                    }

                    // Basic movement: hjkl (with count)
                    KeyCode::Char('h') | KeyCode::Left => {
                        for _ in 0..count {
                            copy_state.move_left();
                        }
                        copy_state.reset_count();
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        for _ in 0..count {
                            copy_state.move_right();
                        }
                        copy_state.reset_count();
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        for _ in 0..count {
                            copy_state.move_up();
                        }
                        copy_state.reset_count();
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        for _ in 0..count {
                            copy_state.move_down();
                        }
                        copy_state.reset_count();
                    }

                    // Line navigation: 0 (when no count), $, ^
                    KeyCode::Char('0') => {
                        copy_state.move_to_line_start();
                        copy_state.reset_count();
                    }
                    KeyCode::Char('$') => {
                        do_line_end = true;
                    }
                    KeyCode::Char('^') => {
                        do_first_non_blank = true;
                    }

                    // Word motions: w, W, b, B, e, E (with count)
                    KeyCode::Char('w') => {
                        do_word_motion = Some((true, false, false)); // forward, not end, small word
                    }
                    KeyCode::Char('W') => {
                        do_word_motion = Some((true, false, true)); // forward, not end, big WORD
                    }
                    KeyCode::Char('b') => {
                        do_word_motion = Some((false, false, false)); // backward, small word
                    }
                    KeyCode::Char('B') => {
                        do_word_motion = Some((false, false, true)); // backward, big WORD
                    }
                    KeyCode::Char('e') => {
                        do_word_motion = Some((true, true, false)); // forward, end, small word
                    }
                    KeyCode::Char('E') => {
                        do_word_motion = Some((true, true, true)); // forward, end, big WORD
                    }

                    // Buffer navigation: gg, G
                    KeyCode::Char('g') => {
                        copy_state.move_to_top();
                        copy_state.reset_count();
                    }
                    KeyCode::Char('G') => {
                        copy_state.move_to_bottom();
                        copy_state.reset_count();
                    }

                    // Screen navigation: H, M, L
                    KeyCode::Char('H') => {
                        copy_state.move_to_screen_top();
                        copy_state.reset_count();
                    }
                    KeyCode::Char('M') => {
                        copy_state.move_to_screen_middle();
                        copy_state.reset_count();
                    }
                    KeyCode::Char('L') => {
                        copy_state.move_to_screen_bottom();
                        copy_state.reset_count();
                    }

                    // Page navigation
                    KeyCode::PageUp => {
                        for _ in 0..count {
                            copy_state.page_up();
                        }
                        copy_state.reset_count();
                    }
                    KeyCode::PageDown => {
                        for _ in 0..count {
                            copy_state.page_down();
                        }
                        copy_state.reset_count();
                    }

                    // Visual modes
                    KeyCode::Char('v') => {
                        copy_state.toggle_visual_char();
                        copy_state.reset_count();
                    }
                    KeyCode::Char('V') => {
                        copy_state.toggle_visual_line();
                        copy_state.reset_count();
                    }

                    // Yank
                    KeyCode::Char('y') => {
                        if copy_state.visual_mode != copy_mode::VisualMode::None {
                            yank_selection = true;
                        }
                        copy_state.reset_count();
                    }

                    // Search: /, ?
                    KeyCode::Char('/') => {
                        copy_state.start_search(true);
                    }
                    KeyCode::Char('?') => {
                        copy_state.start_search(false);
                    }

                    // Search next/prev: n, N
                    KeyCode::Char('n') => {
                        do_search_next = true;
                    }
                    KeyCode::Char('N') => {
                        do_search_prev = true;
                    }

                    // Find char: f, F, t, T
                    KeyCode::Char('f') => {
                        do_find_char = Some((true, true)); // forward, inclusive
                    }
                    KeyCode::Char('F') => {
                        do_find_char = Some((false, true)); // backward, inclusive
                    }
                    KeyCode::Char('t') => {
                        do_find_char = Some((true, false)); // forward, till (not inclusive)
                    }
                    KeyCode::Char('T') => {
                        do_find_char = Some((false, false)); // backward, till
                    }

                    // Repeat find: ;, ,
                    KeyCode::Char(';') => {
                        do_repeat_find = true;
                    }
                    KeyCode::Char(',') => {
                        do_repeat_find_reverse = true;
                    }

                    // Text objects: i (inner), a (around)
                    KeyCode::Char('i') => {
                        copy_state.start_text_object(copy_mode::TextObjectModifier::Inner);
                    }
                    KeyCode::Char('a') => {
                        copy_state.start_text_object(copy_mode::TextObjectModifier::Around);
                    }

                    _ => {
                        copy_state.reset_count();
                    }
                }
            }

            // Handle motions that need line content (separate borrow)
            if do_line_end || do_first_non_blank || do_word_motion.is_some() {
                if let Some(pane) = self.panes.focused() {
                    if let Some(buffer) = self.buffers.get(&pane.id) {
                        if let Some(ref copy_state) = self.copy_mode {
                            let count = copy_state.get_count();
                            let cursor_y = copy_state.cursor.y;
                            let line = self.get_line_content(buffer, cursor_y);
                            if let Some(ref mut cs) = self.copy_mode {
                                if do_line_end {
                                    cs.move_to_line_end(&line);
                                } else if do_first_non_blank {
                                    cs.move_to_first_non_blank(&line);
                                } else if let Some((forward, end, big_word)) = do_word_motion {
                                    for _ in 0..count {
                                        // Re-fetch line content in case cursor moved to different line
                                        // (word motions currently stay on same line, but be safe)
                                        if forward && !end {
                                            cs.move_word_forward(&line, big_word);
                                        } else if !forward {
                                            cs.move_word_backward(&line, big_word);
                                        } else {
                                            cs.move_word_end(&line, big_word);
                                        }
                                    }
                                }
                                cs.reset_count();
                            }
                        }
                    }
                }
            }

            // Handle search operations
            if do_search_next || do_search_prev {
                if let Some(pane) = self.panes.focused() {
                    if let Some(buffer) = self.buffers.get(&pane.id) {
                        let get_line = |y: i32| get_line_content_static(buffer, y);
                        if let Some(ref mut cs) = self.copy_mode {
                            if do_search_next {
                                cs.search_next(get_line);
                            } else {
                                cs.search_prev(get_line);
                            }
                            cs.reset_count();
                        }
                    }
                }
            }

            // Handle find char start
            if let Some((forward, inclusive)) = do_find_char {
                if let Some(ref mut cs) = self.copy_mode {
                    cs.start_find_char(forward, inclusive);
                }
            }

            // Handle repeat find
            if do_repeat_find || do_repeat_find_reverse {
                if let Some(pane) = self.panes.focused() {
                    if let Some(buffer) = self.buffers.get(&pane.id) {
                        if let Some(ref copy_state) = self.copy_mode {
                            let line = self.get_line_content(buffer, copy_state.cursor.y);
                            if let Some(ref mut cs) = self.copy_mode {
                                if do_repeat_find {
                                    cs.repeat_find(&line);
                                } else {
                                    cs.repeat_find_reverse(&line);
                                }
                                cs.reset_count();
                            }
                        }
                    }
                }
            }

            // Handle yank (needs to be done after match to avoid borrow issues)
            if yank_selection {
                if let Some(text) = self.extract_copy_mode_selection() {
                    if let Err(e) = self.copy_to_clipboard(&text) {
                        log::warn!("clipboard copy failed: {}", e);
                    }
                }
                exit_copy_mode = true;
            }

            if exit_copy_mode {
                self.copy_mode = None;
            }

            self.compositor.invalidate();
            self.needs_redraw = true;
            return;
        }

        // Forward input to pane(s)
        let bytes = key_event_to_bytes(&key);
        if self.broadcast_mode {
            // Send to all visible panes
            let visible_ids = self.panes.visible_in_view(self.current_view);
            for id in visible_ids {
                if let Some(pane) = self.panes.get_mut(id) {
                    write_to_pane(pane, &bytes);
                }
            }
        } else {
            // Send to focused pane only
            if let Some(pane) = self.panes.focused_mut() {
                write_to_pane(pane, &bytes);
            }
        }
    }

    /// Handle mouse input
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let x = mouse.column;
        let y = mouse.row;

        if self.forward_mouse_to_pane(&mouse) {
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Find which pane was clicked
                if let Some((pane_id, buf_x, buf_y)) = self.pane_at_position(x, y) {
                    // Clear old selection by invalidating compositor
                    if self.mouse_selection.is_some() {
                        self.compositor.invalidate();
                    }
                    // Start selection
                    self.mouse_selection = Some(MouseSelection {
                        pane_id,
                        buf_start_x: buf_x,
                        buf_start_y: buf_y,
                        buf_end_x: buf_x,
                        buf_end_y: buf_y,
                    });
                    // Focus the clicked pane
                    self.panes.focus_by_id(pane_id);
                    self.save_tag_state();
                    self.needs_redraw = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Extend selection - check position first, then update selection
                let pos_info = self.pane_at_position(x, y);
                if let Some(ref mut sel) = self.mouse_selection {
                    if let Some((pane_id, buf_x, buf_y)) = pos_info {
                        if pane_id == sel.pane_id {
                            sel.buf_end_x = buf_x;
                            sel.buf_end_y = buf_y;
                            self.needs_redraw = true;
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Finish selection and copy to clipboard (but keep selection visible)
                if let Some(ref sel) = self.mouse_selection {
                    if sel.buf_start_x != sel.buf_end_x || sel.buf_start_y != sel.buf_end_y {
                        // Extract selected text and copy
                        if let Some(text) = self.extract_selection(sel) {
                            if let Err(e) = self.copy_to_clipboard(&text) {
                                log::warn!("clipboard copy failed: {}", e);
                            }
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                // Scroll focused pane up (into history) - enters copy mode if not already
                if let Some(pane) = self.panes.focused() {
                    if let Some(buffer) = self.buffers.get(&pane.id) {
                        if self.copy_mode.is_none() {
                            self.copy_mode = Some(CopyModeState::new(
                                buffer.width(),
                                buffer.height(),
                                buffer.scrollback_len(),
                            ));
                        }
                        if let Some(ref mut copy_state) = self.copy_mode {
                            let max_offset = copy_state.scrollback_len;
                            if copy_state.scroll_offset < max_offset {
                                copy_state.scroll_offset += 3;
                                copy_state.scroll_offset = copy_state.scroll_offset.min(max_offset);
                                self.compositor.invalidate();
                                self.needs_redraw = true;
                            }
                        }
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                // Scroll focused pane down (towards live)
                if let Some(ref mut copy_state) = self.copy_mode {
                    if copy_state.scroll_offset > 0 {
                        copy_state.scroll_offset = copy_state.scroll_offset.saturating_sub(3);
                        if copy_state.scroll_offset == 0 {
                            self.copy_mode = None;
                        }
                        self.compositor.invalidate();
                        self.needs_redraw = true;
                    }
                }
            }
            _ => {}
        }
    }

    /// Forward a mouse event to the pane under the pointer if its
    /// application enabled mouse tracking. Returns true when the event was
    /// consumed. Shift bypasses forwarding so truetm's own selection and
    /// scrollback stay reachable (same convention as st, xterm, tmux), and
    /// an active copy mode keeps the mouse for scrollback navigation.
    fn forward_mouse_to_pane(&mut self, mouse: &MouseEvent) -> bool {
        if mouse.modifiers.contains(KeyModifiers::SHIFT) || self.copy_mode.is_some() {
            return false;
        }
        let Some((pane_id, buf_x, buf_y)) = self.pane_at_position(mouse.column, mouse.row) else {
            return false;
        };
        let Some(buffer) = self.buffers.get(&pane_id) else {
            return false;
        };
        let tracking = buffer.mouse_tracking();
        if !tracking.wants(mouse.kind) {
            return false;
        }
        let Some(bytes) = mouse::encode(mouse.kind, mouse.modifiers, buf_x, buf_y, buffer.mouse_sgr()) else {
            return true; // wanted but unencodable: swallow rather than misroute
        };

        // Clicking a pane focuses it, as it does without mouse tracking
        if let MouseEventKind::Down(_) = mouse.kind {
            if self.panes.focused().map(|p| p.id) != Some(pane_id) {
                self.panes.focus_by_id(pane_id);
                self.save_tag_state();
                self.needs_redraw = true;
            }
        }

        if let Some(pane) = self.panes.get_mut(pane_id) {
            write_to_pane(pane, &bytes);
        }
        true
    }

    /// Find which pane contains a screen position, return pane ID and buffer coordinates
    fn pane_at_position(&self, x: u16, y: u16) -> Option<(PaneId, u16, u16)> {
        let visible_ids = self.panes.visible_in_view(self.current_view);
        for &pane_id in &visible_ids {
            if let Some(pane) = self.panes.get(pane_id) {
                // Check if position is within pane's content area (excluding header)
                let content_y_start = pane.rect.y + 1; // Skip header row
                let content_y_end = pane.rect.y + pane.rect.height;
                if x >= pane.rect.x && x < pane.rect.x + pane.rect.width
                    && y >= content_y_start && y < content_y_end
                {
                    let buf_x = x - pane.rect.x;
                    let buf_y = y - content_y_start;
                    return Some((pane_id, buf_x, buf_y));
                }
            }
        }
        None
    }

    /// Extract selected text from a pane's buffer
    fn extract_selection(&self, sel: &MouseSelection) -> Option<String> {
        let buffer = self.buffers.get(&sel.pane_id)?;

        // Normalize selection (ensure start <= end)
        let (start_y, start_x, end_y, end_x) = if sel.buf_start_y < sel.buf_end_y
            || (sel.buf_start_y == sel.buf_end_y && sel.buf_start_x <= sel.buf_end_x)
        {
            (sel.buf_start_y, sel.buf_start_x, sel.buf_end_y, sel.buf_end_x)
        } else {
            (sel.buf_end_y, sel.buf_end_x, sel.buf_start_y, sel.buf_start_x)
        };

        let mut result = String::new();
        for y in start_y..=end_y {
            let line_start = if y == start_y { start_x } else { 0 };
            let line_end = if y == end_y { end_x } else { buffer.width().saturating_sub(1) };

            for x in line_start..=line_end {
                let cell = buffer.get(x, y);
                if cell.ch != render::WIDE_CONT {
                    result.push(cell.ch);
                }
            }
            // Trim trailing spaces from each line
            if y < end_y {
                while result.ends_with(' ') {
                    result.pop();
                }
                result.push('\n');
            }
        }
        // Trim trailing spaces from last line
        while result.ends_with(' ') {
            result.pop();
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    /// Copy text to clipboard via xclip (fallback: OSC 52)
    fn copy_to_clipboard(&mut self, text: &str) -> Result<()> {
        use std::process::{Command, Stdio};

        // Try xclip first (works reliably on X11)
        if let Ok(mut child) = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
                drop(stdin); // Close stdin so xclip can read
            }
            let _ = child.wait();
            return Ok(());
        }

        // Try xsel as fallback
        if let Ok(mut child) = Command::new("xsel")
            .args(["--clipboard", "--input"])
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
                drop(stdin);
            }
            let _ = child.wait();
            return Ok(());
        }

        // Last resort: OSC 52 (requires terminal support)
        let encoded = base64_encode(text);
        let osc52 = format!("\x1b]52;c;{}\x07", encoded);
        let mut stdout = io::stdout();
        write!(stdout, "{}", osc52)?;
        stdout.flush()?;

        Ok(())
    }

    /// Get line content from buffer at given buffer Y coordinate (negative = scrollback)
    fn get_line_content(&self, buffer: &ScreenBuffer, buffer_y: i32) -> Vec<char> {
        get_line_content_static(buffer, buffer_y)
    }
}

/// Write to a pane's PTY, logging failures instead of propagating them.
/// Writes race against the shell exiting (the Exit message may not be
/// processed yet); a failed write must never take down the session.
fn write_to_pane(pane: &mut Pane, bytes: &[u8]) {
    if let Err(e) = pane.write(bytes) {
        log::warn!("pane {:?}: PTY write failed: {}", pane.id, e);
    }
}

/// Static helper to get line content (avoids borrow issues in closures)
fn get_line_content_static(buffer: &ScreenBuffer, buffer_y: i32) -> Vec<char> {
    // Continuation cells of wide chars become spaces so that indices in the
    // returned Vec keep mapping 1:1 to buffer columns (copy mode relies on it).
    let mut line = Vec::new();
    for x in 0..buffer.width() {
        let cell = buffer.get_at_scroll_offset(x, buffer_y);
        line.push(if cell.ch == render::WIDE_CONT { ' ' } else { cell.ch });
    }
    line
}

impl App {

    /// Extract selected text from copy mode selection
    fn extract_copy_mode_selection(&self) -> Option<String> {
        let copy_state = self.copy_mode.as_ref()?;
        let (start, end) = copy_state.get_selection_bounds()
            .map(|(sx, sy, ex, ey)| (
                copy_mode::BufferPos::new(sx, sy),
                copy_mode::BufferPos::new(ex, ey),
            ))?;

        let pane = self.panes.focused()?;
        let buffer = self.buffers.get(&pane.id)?;

        let mut result = String::new();
        for y in start.y..=end.y {
            let line_start = if y == start.y { start.x } else { 0 };
            let line_end = if y == end.y { end.x } else { buffer.width().saturating_sub(1) };

            for x in line_start..=line_end {
                let cell = buffer.get_at_scroll_offset(x, y);
                if cell.ch != render::WIDE_CONT {
                    result.push(cell.ch);
                }
            }
            // Trim trailing spaces from each line
            if y < end.y {
                while result.ends_with(' ') {
                    result.pop();
                }
                result.push('\n');
            }
        }
        // Trim trailing spaces from last line
        while result.ends_with(' ') {
            result.pop();
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    /// Render the screen
    fn render(&mut self) -> Result<()> {
        if !self.needs_redraw {
            return Ok(());
        }

        let mut stdout = io::stdout();

        // Hide cursor during rendering to avoid ghost cursor
        queue!(stdout, Hide)?;

        // Get visible panes and focused pane
        let visible_ids = self.panes.visible_in_view(self.current_view);
        let focused_id = self.panes.focused().map(|p| p.id);

        // Render only visible panes with window numbers
        for (win_num, &pane_id) in visible_ids.iter().enumerate() {
            if let Some(pane) = self.panes.get(pane_id) {
                // Skip panes with zero-size rect (hidden in monocle mode)
                if pane.rect.width == 0 || pane.rect.height == 0 {
                    continue;
                }
                if let Some(buffer) = self.buffers.get(&pane.id) {
                    // In broadcast mode, all panes are "active"
                    let is_focused = self.broadcast_mode || Some(pane.id) == focused_id;
                    // Content rect starts at y+1 to leave room for header
                    let content_rect = Rect::new(
                        pane.rect.x,
                        pane.rect.y + 1,
                        pane.rect.width,
                        pane.rect.height.saturating_sub(1),
                    );
                    // Only apply scroll offset to focused pane in copy mode
                    let offset = if is_focused {
                        self.copy_mode.as_ref().map(|c| c.scroll_offset).unwrap_or(0)
                    } else {
                        0
                    };
                    // Get selection bounds for this pane (mouse selection or copy mode selection)
                    let selection = if is_focused {
                        if let Some(ref copy_state) = self.copy_mode {
                            copy_state.get_selection_bounds().map(|(sx, sy, ex, ey)| {
                                // Convert buffer Y to screen Y for rendering
                                // sy/ey are buffer coords (negative = scrollback)
                                // Need to convert to screen coords relative to visible area
                                let screen_sy = (sy + copy_state.scroll_offset as i32) as u16;
                                let screen_ey = (ey + copy_state.scroll_offset as i32) as u16;
                                (sx, screen_sy, ex, screen_ey)
                            })
                        } else {
                            self.mouse_selection.as_ref()
                                .filter(|s| s.pane_id == pane.id)
                                .map(|s| (s.buf_start_x, s.buf_start_y, s.buf_end_x, s.buf_end_y))
                        }
                    } else {
                        self.mouse_selection.as_ref()
                            .filter(|s| s.pane_id == pane.id)
                            .map(|s| (s.buf_start_x, s.buf_start_y, s.buf_end_x, s.buf_end_y))
                    };
                    // Get search matches for this pane (convert to screen coordinates)
                    let search_matches: Vec<(u16, u16, u16)> = if is_focused {
                        self.copy_mode.as_ref()
                            .map(|cs| {
                                cs.search_matches.iter()
                                    .filter_map(|m| {
                                        // Convert buffer Y to screen Y
                                        let screen_y = (m.y + cs.scroll_offset as i32) as i16;
                                        if screen_y >= 0 && screen_y < buffer.height() as i16 {
                                            Some((m.x, screen_y as u16, m.len))
                                        } else {
                                            None
                                        }
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    self.compositor.render_pane(&mut stdout, buffer, content_rect, is_focused, offset, selection, &search_matches)?;
                    // Draw window header with number and title
                    // Show copy mode indicator in header of focused pane
                    let mode_indicator = if is_focused {
                        self.copy_mode.as_ref().map(|cs| {
                            match cs.visual_mode {
                                copy_mode::VisualMode::None => "COPY",
                                copy_mode::VisualMode::Char => "VISUAL",
                                copy_mode::VisualMode::Line => "V-LINE",
                            }
                        })
                    } else {
                        None
                    };
                    self.draw_window_header(&mut stdout, pane.rect, win_num + 1, buffer.title(), is_focused, mode_indicator)?;
                }
            }
        }

        // Draw separators between visible panes
        let content_height = self.height.saturating_sub(1);
        if visible_ids.len() > 1 && self.layout.current_name() == "[]=" {
            // Find the master pane (first visible)
            if let Some(&master_id) = visible_ids.first() {
                if let Some(master) = self.panes.get(master_id) {
                    let sep_x = master.rect.x + master.rect.width;
                    if sep_x < self.width {
                        queue!(stdout, ResetColor)?;
                        for y in 0..content_height {
                            queue!(stdout, MoveTo(sep_x, y))?;
                            write!(stdout, "│")?;
                        }
                    }
                }
            }
        }

        // Render status bar at bottom
        self.render_status_bar(&mut stdout)?;

        // Position cursor in focused pane (if visible and has size)
        if let Some(pane) = self.panes.focused() {
            if visible_ids.contains(&pane.id) && pane.rect.width > 0 && pane.rect.height > 0 {
                if let Some(ref copy_state) = self.copy_mode {
                    // In copy mode: show copy mode cursor
                    if let Some((cx, cy)) = copy_state.cursor_screen_pos() {
                        // +1 to account for header row
                        queue!(stdout, MoveTo(pane.rect.x + cx, pane.rect.y + 1 + cy))?;
                        queue!(stdout, Show)?;
                    }
                } else if let Some(buffer) = self.buffers.get(&pane.id) {
                    // Normal mode: show buffer cursor
                    let (cx, cy) = buffer.cursor();
                    // Clamp cursor to content area bounds
                    let content_height = pane.rect.height.saturating_sub(1);
                    let clamped_cx = cx.min(pane.rect.width.saturating_sub(1));
                    let clamped_cy = cy.min(content_height.saturating_sub(1));
                    // +1 to account for header row
                    queue!(stdout, MoveTo(pane.rect.x + clamped_cx, pane.rect.y + 1 + clamped_cy))?;
                    // Forward the focused pane's DECSCUSR cursor style to the host terminal
                    // so e.g. vim's bar cursor in insert mode shows through.
                    write!(stdout, "\x1b[{} q", buffer.cursor_style())?;
                    // Only show cursor if the application wants it visible
                    if buffer.cursor_visible() {
                        queue!(stdout, Show)?;
                    }
                }
            }
        }

        stdout.flush()?;
        self.needs_redraw = false;

        Ok(())
    }

    /// Render the status bar showing tags
    fn render_status_bar(&self, stdout: &mut impl Write) -> Result<()> {
        use crossterm::style::{Attribute, SetAttribute, SetForegroundColor, Color};

        let status_y = self.height.saturating_sub(1);
        queue!(stdout, MoveTo(0, status_y), ResetColor)?;

        // Clear the status line
        write!(stdout, "{:width$}", "", width = self.width as usize)?;
        queue!(stdout, MoveTo(0, status_y))?;

        // Initial padding
        write!(stdout, " ")?;
        let mut used = 1usize;

        // Leave room for the layout name and the indicators drawn after the tags
        let budget = (self.width as usize)
            .saturating_sub(self.layout.current_name().chars().count() + 8);

        // Render only tags that have panes or are currently viewed
        for tag in 0..self.tag_count {
            let has_panes = self.panes.any_with_tag(tag);
            let is_viewed = self.current_view.contains(tag);

            // Skip tags with no panes and not currently viewed
            if !has_panes && !is_viewed {
                continue;
            }

            let is_focused_tag = self.panes.focused()
                .map(|p| p.tags.contains(tag))
                .unwrap_or(false);

            // Style based on state - muted green theme
            if is_viewed && is_focused_tag {
                // Viewed and focused pane has this tag - brighter green bold
                queue!(stdout, SetForegroundColor(Color::Rgb { r: 120, g: 190, b: 120 }), SetAttribute(Attribute::Bold))?;
            } else if is_viewed {
                // Currently viewing this tag - muted green
                queue!(stdout, SetForegroundColor(Color::Rgb { r: 80, g: 150, b: 80 }))?;
            } else {
                // Has panes but not viewing - dim green
                queue!(stdout, SetForegroundColor(Color::Rgb { r: 60, g: 100, b: 60 }))?;
            }

            // Show "N (folder)", with "x2" appended when the tag holds more
            // than one window. Falls back to the bare number rather than
            // wrapping the status line.
            let number = (tag + 1).to_string();
            let labelled = self.tag_info.get(&tag).and_then(|info| {
                let label = tag::trim_label(&info.label, tag::LABEL_MAX_WIDTH);
                let count = if info.panes > 1 {
                    format!("\u{d7}{}", info.panes)
                } else {
                    String::new()
                };
                let width = number.len()
                    + tag::display_width(&label)
                    + tag::display_width(&count)
                    + 3;
                (used + width + 2 <= budget)
                    .then(|| (format!("{} ({}{})", number, label, count), width))
            });
            let (text, width) = labelled.unwrap_or_else(|| (number.clone(), number.len()));

            write!(stdout, "{}", text)?;
            queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
            write!(stdout, "  ")?;
            used += width + 2;
        }

        // Show layout name
        queue!(stdout, SetForegroundColor(Color::DarkGrey))?;
        write!(stdout, "{}", self.layout.current_name())?;

        // Show zoom indicator
        if self.zoomed_pane.is_some() {
            queue!(stdout, SetForegroundColor(Color::Magenta), SetAttribute(Attribute::Bold))?;
            write!(stdout, " [Z]")?;
        }

        // Show copy mode prompts in status bar (mode indicator is in window header)
        if let Some(ref copy_state) = self.copy_mode {
            queue!(stdout, SetForegroundColor(Color::Yellow), SetAttribute(Attribute::Bold))?;

            // Check for search input mode
            if copy_state.search_mode != copy_mode::SearchMode::None {
                let prompt = if copy_state.search_mode == copy_mode::SearchMode::Forward { "/" } else { "?" };
                write!(stdout, " {}{}", prompt, copy_state.search_input)?;
            } else if copy_state.pending_find.is_some() {
                write!(stdout, " find:")?;
            } else if let Some(ref modifier) = copy_state.pending_text_object {
                let mod_char = match modifier {
                    copy_mode::TextObjectModifier::Inner => 'i',
                    copy_mode::TextObjectModifier::Around => 'a',
                };
                write!(stdout, " {}:", mod_char)?;
            } else if let Some(count) = copy_state.count {
                // Show count if being entered
                write!(stdout, " {}", count)?;
            }
        }

        queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;

        Ok(())
    }

    /// Draw window header with number, title, and line
    fn draw_window_header(&self, stdout: &mut impl Write, rect: Rect, num: usize, title: Option<&str>, is_focused: bool, mode_indicator: Option<&str>) -> Result<()> {
        use crossterm::style::{SetForegroundColor, Color, Attribute, SetAttribute};

        queue!(stdout, MoveTo(rect.x, rect.y))?;

        // Use yellow for entire header when in copy mode, otherwise green/grey
        let in_copy_mode = mode_indicator.is_some();
        if in_copy_mode {
            queue!(stdout, SetForegroundColor(Color::Yellow), SetAttribute(Attribute::Bold))?;
        } else if is_focused {
            queue!(stdout, SetForegroundColor(Color::Rgb { r: 120, g: 190, b: 120 }), SetAttribute(Attribute::Bold))?;
        } else {
            queue!(stdout, SetForegroundColor(Color::DarkGrey))?;
        }

        // Draw dashes and window number
        let num_str = format!("──[{}]", num);
        write!(stdout, "{}", num_str)?;

        // Calculate remaining width for title and line
        let mut remaining = rect.width.saturating_sub(num_str.chars().count() as u16) as usize;

        // Reserve space for mode indicator if present
        let indicator_len = mode_indicator.map(|s| s.len() + 2).unwrap_or(0); // +2 for brackets
        let available_for_title = remaining.saturating_sub(indicator_len);

        // Draw title if present
        if let Some(title) = title {
            if available_for_title > 2 {
                use unicode_width::UnicodeWidthChar;
                write!(stdout, " ")?;
                remaining -= 1;
                // Truncate by display width (titles can contain multi-byte
                // and double-width characters), leaving room for the
                // trailing line and indicator
                let max_title_width = available_for_title.saturating_sub(2);
                let title_width: usize = title.chars()
                    .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                    .sum();
                if title_width <= max_title_width {
                    write!(stdout, "{}", title)?;
                    remaining = remaining.saturating_sub(title_width);
                } else if max_title_width > 3 {
                    let mut written = 0usize;
                    for c in title.chars() {
                        let w = UnicodeWidthChar::width(c).unwrap_or(0);
                        if written + w > max_title_width - 1 {
                            break;
                        }
                        write!(stdout, "{}", c)?;
                        written += w;
                    }
                    write!(stdout, "…")?;
                    remaining = remaining.saturating_sub(written + 1);
                }
                write!(stdout, " ")?;
                remaining = remaining.saturating_sub(1);
            }
        }

        // Draw line, leaving room for indicator
        let line_len = remaining.saturating_sub(indicator_len);
        for _ in 0..line_len {
            write!(stdout, "─")?;
        }

        // Draw mode indicator at the end if present
        if let Some(indicator) = mode_indicator {
            write!(stdout, "[{}]", indicator)?;
        }

        queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;

        Ok(())
    }
}

/// What the status bar shows for one tag.
#[derive(PartialEq, Eq)]
struct TagInfo {
    /// Working directory of the tag's master pane
    label: String,
    /// How many windows carry the tag
    panes: usize,
}

/// Short name for a directory: its basename, with `~` for home and `/` for root.
fn dir_label(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if path == Path::new(&home) {
            return "~".to_string();
        }
    }
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.to_string_lossy().into_owned(),
    }
}

fn run() -> Result<()> {

    let (width, height) = terminal::size().context("Failed to get terminal size")?;

    let mut app = App::new(width, height);

    // Create initial pane
    app.create_pane()?;
    app.refresh_tag_info();

    // Set up terminal
    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen, SetTitle("truetm"))?;

    // Enable Kitty keyboard protocol for unambiguous key handling (Alt+O etc.)
    // This is supported by Kitty, Foot, WezTerm, Alacritty, and others
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );

    // Enable mouse capture for pane-isolated selection
    execute!(io::stdout(), EnableMouseCapture)?;

    // Frame timing - target ~60fps max, but render immediately if idle
    let frame_duration = Duration::from_micros(16667); // ~60fps
    let mut last_render = Instant::now();

    // Tag labels follow the shell's cwd, so they need re-reading on a timer
    let label_interval = Duration::from_millis(250);
    let mut last_labels = Instant::now();

    // Main loop
    while app.running {
        // Process all available PTY output first
        let had_pty_data = app.process_pty_messages();

        // Check for input with adaptive timeout:
        // - If we have pending renders and frame time elapsed, render now (0ms timeout)
        // - If PTY data is flowing, use short timeout to stay responsive
        // - Otherwise use longer timeout to reduce CPU usage
        let now = Instant::now();
        let since_render = now.duration_since(last_render);

        if now.duration_since(last_labels) >= label_interval {
            last_labels = now;
            if app.refresh_tag_info() {
                app.needs_redraw = true;
            }
        }

        let poll_timeout = if app.needs_redraw && since_render >= frame_duration {
            Duration::ZERO // Render immediately
        } else if had_pty_data {
            Duration::from_micros(500) // Quick check for more data
        } else {
            Duration::from_millis(16) // Idle - save CPU
        };

        if event::poll(poll_timeout)? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::Resize(w, h) => app.resize(w, h),
                _ => {}
            }
        }

        // Render if needed and frame time has elapsed (or if we're idle)
        if app.needs_redraw {
            let now = Instant::now();
            if now.duration_since(last_render) >= frame_duration || !had_pty_data {
                app.render()?;
                last_render = now;
            }
        }

        // Exit if no panes left
        if app.panes.is_empty() {
            app.running = false;
        }
    }

    Ok(())
}

/// Convert a crossterm KeyEvent to bytes to send to PTY
fn key_event_to_bytes(key: &KeyEvent) -> Vec<u8> {
    // Calculate xterm modifier code: 1 + (shift?1:0) + (alt?2:0) + (ctrl?4:0)
    // Result: 2=Shift, 3=Alt, 4=Shift+Alt, 5=Ctrl, 6=Shift+Ctrl, 7=Alt+Ctrl, 8=all
    let modifier = {
        let mut m = 1u8;
        if key.modifiers.contains(KeyModifiers::SHIFT) { m += 1; }
        if key.modifiers.contains(KeyModifiers::ALT) { m += 2; }
        if key.modifiers.contains(KeyModifiers::CONTROL) { m += 4; }
        m
    };
    let has_modifier = modifier > 1;

    match key.code {
        KeyCode::Char(c) => {
            let mut bytes = Vec::new();
            // Alt adds ESC prefix
            if key.modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+letter produces control codes 1-26
                if c.is_ascii_alphabetic() {
                    bytes.push((c.to_ascii_lowercase() as u8) - b'a' + 1);
                } else {
                    match c {
                        '[' | '3' => bytes.push(0x1b), // Ctrl+[ = ESC
                        '\\' | '4' => bytes.push(0x1c),
                        ']' | '5' => bytes.push(0x1d),
                        '^' | '6' => bytes.push(0x1e),
                        '_' | '7' => bytes.push(0x1f),
                        '?' | '8' => bytes.push(0x7f), // Ctrl+? = DEL
                        '@' | '2' => bytes.push(0x00), // Ctrl+@ = NUL
                        _ => bytes.extend(c.to_string().as_bytes()),
                    }
                }
            } else {
                bytes.extend(c.to_string().as_bytes());
            }
            bytes
        }
        KeyCode::Enter => {
            // Modifiers on Enter: some apps expect CSI sequences
            if has_modifier {
                format!("\x1b[13;{}u", modifier).into_bytes()
            } else {
                vec![b'\r']
            }
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                b"\x1b[9;5u".to_vec() // Ctrl+Tab
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                b"\x1b[9;3u".to_vec() // Alt+Tab (if it reaches us)
            } else {
                vec![b'\t']
            }
        }
        KeyCode::BackTab => b"\x1b[Z".to_vec(), // Shift+Tab
        KeyCode::Backspace => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                vec![0x08] // Ctrl+Backspace: often BS (0x08) or delete word
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                b"\x1b\x7f".to_vec() // Alt+Backspace
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Esc => {
            if key.modifiers.contains(KeyModifiers::ALT) {
                b"\x1b\x1b".to_vec() // Alt+Esc
            } else {
                vec![0x1b]
            }
        }

        // Navigation keys with modifier support (xterm format: CSI 1;modifier X)
        KeyCode::Up => csi_with_modifier(b'A', modifier, has_modifier),
        KeyCode::Down => csi_with_modifier(b'B', modifier, has_modifier),
        KeyCode::Right => csi_with_modifier(b'C', modifier, has_modifier),
        KeyCode::Left => csi_with_modifier(b'D', modifier, has_modifier),
        KeyCode::Home => csi_with_modifier(b'H', modifier, has_modifier),
        KeyCode::End => csi_with_modifier(b'F', modifier, has_modifier),

        // Keys with ~ suffix use different format: CSI number;modifier ~
        KeyCode::Insert => csi_tilde_with_modifier(2, modifier, has_modifier),
        KeyCode::Delete => csi_tilde_with_modifier(3, modifier, has_modifier),
        KeyCode::PageUp => csi_tilde_with_modifier(5, modifier, has_modifier),
        KeyCode::PageDown => csi_tilde_with_modifier(6, modifier, has_modifier),

        // Function keys
        KeyCode::F(n) => f_key_sequence(n, modifier, has_modifier),

        _ => Vec::new(),
    }
}

/// Generate CSI sequence with optional modifier: CSI [1;modifier] final
fn csi_with_modifier(final_byte: u8, modifier: u8, has_modifier: bool) -> Vec<u8> {
    if has_modifier {
        format!("\x1b[1;{}{}", modifier, final_byte as char).into_bytes()
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// Generate CSI number ~ sequence with optional modifier: CSI number[;modifier] ~
fn csi_tilde_with_modifier(number: u8, modifier: u8, has_modifier: bool) -> Vec<u8> {
    if has_modifier {
        format!("\x1b[{};{}~", number, modifier).into_bytes()
    } else {
        format!("\x1b[{}~", number).into_bytes()
    }
}

/// Generate function key sequence with optional modifier
fn f_key_sequence(n: u8, modifier: u8, has_modifier: bool) -> Vec<u8> {
    // F1-F4 use SS3 format (no modifier support in standard), F5+ use CSI format
    match n {
        1..=4 if !has_modifier => {
            vec![0x1b, b'O', b'P' + n - 1] // ESC O P/Q/R/S
        }
        1..=4 => {
            // With modifiers, F1-F4 use CSI format
            let code = match n {
                1 => 11, 2 => 12, 3 => 13, 4 => 14,
                _ => return Vec::new(),
            };
            format!("\x1b[{};{}~", code, modifier).into_bytes()
        }
        5..=12 => {
            let code = match n {
                5 => 15, 6 => 17, 7 => 18, 8 => 19,
                9 => 20, 10 => 21, 11 => 23, 12 => 24,
                _ => return Vec::new(),
            };
            if has_modifier {
                format!("\x1b[{};{}~", code, modifier).into_bytes()
            } else {
                format!("\x1b[{}~", code).into_bytes()
            }
        }
        _ => Vec::new(),
    }
}

/// Simple base64 encoder for OSC 52 clipboard
fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let bytes = input.as_bytes();
    let mut result = String::new();

    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;

        let n = (b0 << 16) | (b1 << 8) | b2;

        result.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        result.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            result.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(ALPHABET[(n & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }

    result
}
