//! How the text looks: folding, word wrap, the font and its size, the
//! theme, and the settings file they come from.

use super::*;

impl EditorView {
    /// Folds or opens the block `line` heads: a chevron click.
    pub(super) fn toggle_fold(&self, line: Option<usize>) {
        let (rows, cols) = self.grid();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let buffer = state.docs.active_mut();
        let line = line.unwrap_or_else(|| buffer.cursor_position().0);
        if !buffer.unfold(line) {
            buffer.fold(line);
        }
        buffer.clamp_scroll(rows, cols);
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// View > Fold (`Some(true)`), Unfold (`Some(false)`) at the caret, or
    /// Fold All (`None`). Fold at a line that does not head a block folds
    /// the block the caret is in.
    pub(super) fn fold_command(&self, fold: Option<bool>) {
        let (rows, cols) = self.grid();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let buffer = state.docs.active_mut();
        let line = buffer.cursor_position().0;
        let done = match fold {
            None => {
                if !buffer.fold_all() {
                    state.message = Some((
                        "Fold All works on files up to 100,000 lines".into(),
                        Instant::now(),
                    ));
                    drop(state);
                    self.request_redraw();
                    return;
                }
                true
            }
            Some(false) => buffer.unfold(line),
            Some(true) => {
                // The nearest line above, or this one, that heads a block
                // containing the caret.
                let head = (0..=line).rev().take(2000).find(|&l| {
                    buffer
                        .fold_range(l)
                        .is_some_and(|(_, b)| l == line || b >= line)
                });
                head.is_some_and(|l| buffer.fold(l))
            }
        };
        if !done {
            state.message = Some(("nothing to fold here".into(), Instant::now()));
        }
        let buffer = state.docs.active_mut();
        buffer.clamp_scroll(rows, cols);
        buffer.scroll_to_cursor(rows, cols);
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// View > Word Wrap: flips wrapping for the active document.
    pub(super) fn toggle_word_wrap(&self) {
        let (rows, cols) = self.grid();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let setting = state.word_wrap;
        let buffer = state.docs.active_mut();
        let on = buffer.wrap.is_none();
        buffer.wrap_choice = Some(on);
        apply_wrap(buffer, setting, cols);
        buffer.scroll_column = 0;
        buffer.scroll_to_cursor(rows, cols);
        state.message = Some((
            if on { "word wrap on" } else { "word wrap off" }.into(),
            Instant::now(),
        ));
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Cmd-= and friends: a new code size, remembered in the settings file.
    pub(super) fn set_font_size(&self, size: f32) {
        use crate::platform::settings::{MAX_FONT_SIZE, MIN_FONT_SIZE, Settings};
        let size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        let font = {
            let Some(state) = self.state() else {
                return;
            };
            if (state.font_size - size).abs() < 0.01 {
                return;
            }
            state.font.clone()
        };
        self.apply_font(font.clone(), size);
        let Some(theme) = self.state().map(|state| state.theme_choice) else {
            return;
        };
        let saved = Settings::save_font(&font, size, theme);
        if let Some(mut state) = self.state_mut() {
            state.message = Some((
                match saved {
                    Ok(()) => format!("font size {}", size as i32),
                    Err(e) => format!("font size {}, not saved: {e}", size as i32),
                },
                Instant::now(),
            ));
        }
    }

    /// crc > Settings: the settings file as a tab, created from the template
    /// when there is none. Saving it applies it.
    /// Cmd-,: the Settings page in the editor column, read fresh.
    pub(super) fn open_settings(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.settings_page.reload();
        state.settings_page.open = true;
        state.settings_page.scroll = 0.0;
        if let Some(page) = &mut state.extensions {
            page.details = false;
        }
        state.mcp.details = false;
        state.palette = None;
        state.completion = None;
    }

    /// What a click on the Settings page does.
    pub(super) fn settings_action(&self, action: crate::platform::settings_page::Action) {
        use crate::platform::settings_page::Action;
        match action {
            Action::Close => {
                if let Some(mut state) = self.state_mut() {
                    state.settings_page.open = false;
                }
            }
            Action::Reload => {
                if let Some(mut state) = self.state_mut() {
                    state.settings_page.reload();
                }
            }
            Action::Open => self.open_settings_file(None),
            Action::Edit(key) => self.open_settings_file(Some(key)),
        }
        self.request_redraw();
        self.pump();
    }

    /// config.toml in a tab, created from the template when there is none,
    /// with the caret on `key`'s line when it is given. The page gives the
    /// column back to the tab.
    pub(super) fn open_settings_file(&self, key: Option<&str>) {
        let path = match crate::platform::settings::Settings::ensure_file() {
            Ok(path) => path,
            Err(e) => {
                if let Some(mut state) = self.state_mut() {
                    state.say(layout::Feedback::Failure, format!("settings: {e}"));
                }
                return;
            }
        };
        // Straight into a tab, without adopting ~/.config/crc as the project
        // the way opening an ordinary file with no project would.
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.settings_page.open = false;
        match state.docs.open(&path) {
            Ok(()) => {
                reveal_active_tab(&mut state);
            }
            Err(e) => {
                state.say(layout::Feedback::Failure, format!("settings: {e}"));
                return;
            }
        }
        let line = key.and_then(|key| {
            crate::platform::settings::line_of(&state.docs.active().rope.to_string(), key)
        });
        drop(state);
        if let Some(line) = line {
            let (rows, cols) = self.grid();
            if let Some(mut state) = self.state_mut() {
                let buffer = state.docs.active_mut();
                buffer.goto_line(line);
                buffer.scroll_to_cursor(rows, cols);
            }
        }
        self.sync_title();
        self.reparse();
    }

    /// The settings file was just saved from a tab: read it back and apply
    /// what changed.
    pub(super) fn apply_settings_file(&self) {
        let (settings, problems) = crate::platform::settings::Settings::load_checked();
        let (font, size) = {
            let Some(state) = self.state() else {
                return;
            };
            (state.font.clone(), state.font_size)
        };
        if settings.font != font || (settings.font_size - size).abs() > 0.01 {
            self.apply_font(settings.font.clone(), settings.font_size);
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.theme_choice = settings.theme;
            state.caret_blink = settings.caret_blink;
            state.format_on_save = settings.format_on_save;
            state.organize_on_save = settings.organize_imports_on_save;
            state.word_wrap = settings.word_wrap;
            state.ssh_auth_sock = settings.ssh_auth_sock.clone();
            state.conflict_side = settings.conflict_side_by_side;
        }
        self.apply_theme();
        if let Some(mut state) = self.state_mut() {
            state.settings_page.reload();
            match crate::platform::settings::Settings::describe_problems(&problems) {
                Some(said) => state.say(layout::Feedback::Failure, said),
                None => state.say(layout::Feedback::Success, "settings applied"),
            }
        }
    }

    /// Whether the view is being shown in the dark appearance.
    pub(super) fn system_is_dark(&self) -> bool {
        use objc2_app_kit::{
            NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
        };
        // The names are AppKit statics, which Rust cannot vouch for; they
        // are the documented constants and never written.
        let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
        let names = NSArray::from_slice(&[aqua, dark]);
        self.effectiveAppearance()
            .bestMatchFromAppearancesWithNames(&names)
            .is_some_and(|best| &*best == dark)
    }

    /// Picks the colour table from the settings and the system, and tells
    /// the window so its own alerts and menus agree.
    pub(super) fn apply_theme(&self) {
        use crate::platform::settings::ThemeChoice;
        use objc2_app_kit::{
            NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
        };
        let Some(choice) = self.state().map(|state| state.theme_choice) else {
            return;
        };
        let dark = match choice {
            ThemeChoice::System => self.system_is_dark(),
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
        };
        if let Some(window) = self.window() {
            let (aqua, dark_aqua) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
            let forced = match choice {
                ThemeChoice::System => None,
                ThemeChoice::Dark => NSAppearance::appearanceNamed(dark_aqua),
                ThemeChoice::Light => NSAppearance::appearanceNamed(aqua),
            };
            window.setAppearance(forced.as_deref());
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if state.theme.is_dark() == dark {
                return;
            }
            state.theme = if dark { Theme::dark() } else { Theme::light() };
        }
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
    }

    /// Rebuilds the atlas for `font` at `size`, keeping the UI at its own
    /// size, and re-clamps every pane's scroll to the rows that fit now.
    pub(super) fn apply_font(&self, font: String, size: f32) {
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor()) as f32;
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let atlas = Atlas::build_with_ui(
                &font,
                size,
                crate::platform::settings::DEFAULT_FONT_SIZE,
                scale,
            );
            state.renderer.replace_atlas(atlas);
            state.font = font;
            state.font_size = size;
        }
        // Fewer or more rows fit now; the scroll positions are re-clamped.
        let (rows, cols) = self.grid();
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let setting = state.word_wrap;
            for docs in all_docs_mut(&mut state) {
                apply_wrap(docs.active_mut(), setting, cols);
                docs.active_mut().clamp_scroll(rows, cols);
            }
        }
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
        self.pump();
    }
}
