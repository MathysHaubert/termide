//! Click handlers for menu-bar dropdowns and their nested submenus.
//!
//! Each top-level menu has its own entry point; the main `handle_mouse_event`
//! dispatches here based on which menu is currently open.

use anyhow::Result;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use termide_i18n as i18n;
use termide_theme::Theme;
use termide_ui_render::{
    dropdown_width, get_bookmarks_group_items, get_bookmarks_items, get_commands_group_items,
    get_commands_items, get_menu_item_x_position, get_options_items, get_sessions_items,
    get_shell_items, get_tools_items, BOOKMARKS_MENU_INDEX, COMMANDS_MENU_INDEX,
    OPTIONS_MENU_INDEX, SESSIONS_MENU_INDEX, WINDOWS_MENU_INDEX,
};

/// Hit-test a dropdown menu and return the clicked item index (if any).
///
/// `menu_x` is the left edge of the dropdown, `dropdown_y` is the top row.
/// Returns `Some(index)` if the click is on a valid item, `None` otherwise.
pub(in crate::app) fn hit_dropdown_item(
    x: u16,
    y: u16,
    menu_x: u16,
    dropdown_y: u16,
    items: &[termide_ui_render::DropdownItem],
) -> Option<usize> {
    let width = dropdown_width(items);
    let height = items.len() as u16 + 2; // +2 for borders
    if x >= menu_x && x < menu_x + width && y >= dropdown_y && y < dropdown_y + height {
        let item_index = y.saturating_sub(dropdown_y + 1) as usize;
        if item_index < items.len() {
            return Some(item_index);
        }
    }
    None
}

impl App {
    /// Handle click on Options submenu dropdown
    /// Returns true if click was handled
    pub(in crate::app) fn handle_submenu_click(&mut self, x: u16, y: u16) -> Result<bool> {
        // Get Options dropdown position
        let menu_x = get_menu_item_x_position(OPTIONS_MENU_INDEX);
        let dropdown_y = 1_u16;

        // Calculate Options dropdown dimensions
        let options_items = get_options_items(
            self.detach_available(),
            Some(&self.state.config.general.keybindings),
        );
        let options_width = dropdown_width(&options_items);
        let options_height = options_items.len() as u16 + 2; // +2 for borders

        // Check if nested submenu (Themes) is open
        if self.state.ui.nested_submenu.open && self.state.ui.options_submenu.selected == 0 {
            // Theme dropdown is to the right of Options dropdown
            let nested_x = menu_x + options_width;
            let nested_y = dropdown_y + 1;

            let theme_names = Theme::all_theme_names();
            let nested_width = theme_names.iter().map(|n| n.width()).max().unwrap_or(10) as u16 + 6;
            // Must match ThemeDropdown::max_visible
            let max_visible = 25;
            let nested_height = theme_names.len().min(max_visible) as u16 + 2;

            // Check click on theme dropdown
            if x >= nested_x
                && x < nested_x + nested_width
                && y >= nested_y
                && y < nested_y + nested_height
            {
                // Calculate scroll offset same as ThemeDropdown
                let scroll_offset = if self.state.ui.nested_submenu.selected >= max_visible {
                    self.state.ui.nested_submenu.selected - max_visible + 1
                } else {
                    0
                };
                let item_y = y.saturating_sub(nested_y + 1); // -1 for top border
                let item_index = scroll_offset + item_y as usize;
                if item_index < theme_names.len() {
                    // Clear preview state - theme is confirmed
                    self.state.ui.theme_preview_original = None;
                    // Apply selected theme
                    if let Some(name) = theme_names.get(item_index) {
                        self.apply_theme(name)?;
                    }
                    self.state.close_menu();
                    return Ok(true);
                }
            }
        }

        // Check if nested submenu (Language) is open
        if self.state.ui.nested_submenu.open && self.state.ui.options_submenu.selected == 1 {
            // Language dropdown is to the right of Options dropdown
            let nested_x = menu_x + options_width;
            let nested_y = dropdown_y + 2; // Language is at index 1

            let languages = i18n::get_language_list();
            let nested_width = languages
                .iter()
                .map(|(_, name)| name.width())
                .max()
                .unwrap_or(10) as u16
                + 4;
            // Must match LanguageDropdown::max_visible
            let max_visible = 15;
            let nested_height = languages.len().min(max_visible) as u16 + 2;

            // Check click on language dropdown
            if x >= nested_x
                && x < nested_x + nested_width
                && y >= nested_y
                && y < nested_y + nested_height
            {
                // Calculate scroll offset same as LanguageDropdown
                let scroll_offset = if self.state.ui.nested_submenu.selected >= max_visible {
                    self.state.ui.nested_submenu.selected - max_visible + 1
                } else {
                    0
                };
                let item_y = y.saturating_sub(nested_y + 1); // -1 for top border
                let item_index = scroll_offset + item_y as usize;
                if item_index < languages.len() {
                    // Clear preview state - language is confirmed
                    self.state.ui.language_preview_original = None;
                    // Apply selected language
                    if let Some((code, name)) = languages.get(item_index) {
                        self.apply_language(code, name)?;
                    }
                    self.state.close_menu();
                    return Ok(true);
                }
            }
        }

        // Check click on Options dropdown
        if x >= menu_x
            && x < menu_x + options_width
            && y >= dropdown_y
            && y < dropdown_y + options_height
        {
            let item_y = y.saturating_sub(dropdown_y + 1); // -1 for top border
            let item_index = item_y as usize;
            if item_index < options_items.len() {
                self.state.ui.options_submenu.selected = item_index;
                // Themes and Language toggle their nested dropdown on a second
                // click, which the keyboard path has no equivalent for; every
                // other entry is dispatched by `execute_submenu_action` so that
                // clicking and pressing Enter can never disagree about what an
                // item does.
                match options_items[item_index].key.as_str() {
                    "themes" => {
                        if self.state.ui.nested_submenu.open
                            && self.state.ui.options_submenu.selected == 0
                        {
                            if let Some(original_name) = self.state.ui.theme_preview_original.take()
                            {
                                self.state.theme = Theme::get_by_name(&original_name);
                            }
                            self.state.close_nested_submenu();
                        } else {
                            let theme_names = Theme::all_theme_names();
                            let current_idx = theme_names
                                .iter()
                                .position(|n| n == self.state.theme.name)
                                .unwrap_or(0);
                            self.state.ui.theme_preview_original =
                                Some(self.state.theme.name.to_string());
                            self.state.open_nested_submenu(current_idx);
                        }
                    }
                    "language" => {
                        use termide_i18n as i18n;
                        use termide_ui_render::find_current_language_index;
                        if self.state.ui.nested_submenu.open
                            && self.state.ui.options_submenu.selected == 1
                        {
                            if let Some(original_lang) =
                                self.state.ui.language_preview_original.take()
                            {
                                let _ = i18n::set_language(&original_lang);
                            }
                            self.state.close_nested_submenu();
                        } else {
                            let current_idx = find_current_language_index();
                            self.state.ui.language_preview_original =
                                Some(i18n::current_language());
                            self.state.open_nested_submenu(current_idx);
                        }
                    }
                    _ => self.execute_submenu_action()?,
                }
                return Ok(true);
            }
        }

        // Click outside dropdowns - close all menus
        self.state.close_menu();
        Ok(true)
    }

    /// Handle click on Sessions submenu dropdown
    /// Returns true if click was handled
    pub(in crate::app) fn handle_sessions_submenu_click(&mut self, x: u16, y: u16) -> Result<bool> {
        let menu_x = get_menu_item_x_position(SESSIONS_MENU_INDEX);
        let items = get_sessions_items(Some(&self.state.config.general.keybindings));
        if let Some(index) = hit_dropdown_item(x, y, menu_x, 1, &items) {
            self.state.ui.sessions_submenu.selected = index;
            self.execute_sessions_submenu_action()?;
            return Ok(true);
        }
        self.state.close_menu();
        Ok(true)
    }

    /// Handle click on Tools submenu dropdown
    /// Returns true if click was handled
    pub(in crate::app) fn handle_tools_submenu_click(&mut self, x: u16, y: u16) -> Result<bool> {
        let menu_x = get_menu_item_x_position(WINDOWS_MENU_INDEX);
        let items = get_tools_items(Some(&self.state.config.general.keybindings));

        // If shell picker nested submenu is open, check clicks on it first
        if self.state.ui.tools_nested.open {
            let shell_items = get_shell_items(
                &self.state.cache.shells,
                self.state.config.terminal.default_shell.as_deref(),
            );
            if !shell_items.is_empty() {
                // Calculate nested dropdown position (same formula as in ui.rs rendering)
                let dropdown_y = 1_u16;
                let parent_width = dropdown_width(&items);
                let nested_x = menu_x + parent_width;
                let nested_y = dropdown_y + 1 + self.state.ui.tools_submenu.selected as u16;
                if let Some(index) = hit_dropdown_item(x, y, nested_x, nested_y, &shell_items) {
                    if let Some(shell) = self.state.cache.shells.get(index) {
                        let shell_path = shell.path.clone();
                        // Copy-on-write: mutate in-place if single owner, else clone
                        {
                            let config = Arc::make_mut(&mut self.state.config);
                            config.terminal.default_shell = Some(shell_path.clone());
                        }
                        if let Err(e) = self.save_shell_preference(&shell_path) {
                            log::warn!("Failed to save shell preference: {}", e);
                        }
                        self.state.close_menu();
                        self.handle_new_terminal_with_shell(Some(&shell_path))?;
                        return Ok(true);
                    }
                }
            }
        }

        // Check click on Tools main dropdown
        if let Some(index) = hit_dropdown_item(x, y, menu_x, 1, &items) {
            self.state.ui.tools_submenu.selected = index;
            self.execute_tools_submenu_action()?;
            return Ok(true);
        }
        self.state.close_menu();
        Ok(true)
    }

    /// Handle click on Commands submenu dropdown
    /// Returns true if click was handled
    pub(in crate::app) fn handle_commands_submenu_click(&mut self, x: u16, y: u16) -> Result<bool> {
        let registry = match self.commands_registry() {
            Some(r) => r,
            None => {
                self.state.close_menu();
                return Ok(true);
            }
        };

        // If nested submenu is open, handle clicks on it first
        if self.state.ui.commands_nested.open {
            if let Some(group_name) = self.state.ui.current_commands_group.as_ref() {
                let nested_items = get_commands_group_items(&registry, group_name);
                if !nested_items.is_empty() {
                    let menu_x = get_menu_item_x_position(COMMANDS_MENU_INDEX);
                    let parent_items = get_commands_items(&registry);
                    let parent_width = dropdown_width(&parent_items);
                    let nested_x = menu_x + parent_width;
                    let nested_y = 2 + self.state.ui.commands_submenu.selected as u16;
                    if let Some(index) = hit_dropdown_item(x, y, nested_x, nested_y, &nested_items)
                    {
                        self.state.ui.commands_nested.selected = index;
                        self.execute_commands_nested_action()?;
                        return Ok(true);
                    }
                }
            }
        }

        // Check click on Commands main dropdown
        let menu_x = get_menu_item_x_position(COMMANDS_MENU_INDEX);
        let commands_items = get_commands_items(&registry);
        if let Some(index) = hit_dropdown_item(x, y, menu_x, 1, &commands_items) {
            self.state.ui.commands_submenu.selected = index;
            self.execute_commands_submenu_action()?;
            return Ok(true);
        }

        self.state.close_menu();
        Ok(true)
    }

    /// Handle click on stash dropdown or outside it (close).
    pub(in crate::app) fn handle_stash_dropdown_click(&mut self, x: u16, y: u16) -> Result<()> {
        let items = termide_ui_render::get_stash_items(
            &self.state.stash.entries,
            self.state.stash.has_changes,
        );
        if let Some(btn_area) = self.state.ui.stash_button_area {
            // Calculate actual dropdown position (same clamp logic as Dropdown::render)
            let dropdown_width = items
                .iter()
                .map(|i| i.label.chars().count())
                .max()
                .unwrap_or(0) as u16
                + 6;
            let dropdown_height = items.len().min(20) as u16 + 2;
            let screen_w = self.state.terminal.width;
            let screen_h = self.state.terminal.height;
            let dropdown_x = btn_area.x.min(screen_w.saturating_sub(dropdown_width));
            let dropdown_y = btn_area
                .bottom()
                .min(screen_h.saturating_sub(dropdown_height));

            if let Some(index) = hit_dropdown_item(x, y, dropdown_x, dropdown_y, &items) {
                self.state.ui.stash_submenu.selected = index;
                self.execute_stash_submenu_action()?;
                return Ok(());
            }
        }
        // Click outside → close dropdown
        self.state.ui.stash_submenu.close();
        self.state.needs_redraw = true;
        Ok(())
    }

    /// Handle click on Bookmarks submenu dropdown
    /// Returns true if click was handled
    pub(in crate::app) fn handle_bookmarks_submenu_click(
        &mut self,
        x: u16,
        y: u16,
    ) -> Result<bool> {
        let bookmarks_items = get_bookmarks_items(
            &self.state.bookmarks,
            self.state.project_bookmarks.as_ref(),
            Some(&self.state.config.general.keybindings),
        );

        // If nested submenu is open, handle clicks on it first
        if self.state.ui.bookmarks_nested.open {
            if let Some(group_name) = self.state.ui.current_bookmarks_group.as_ref() {
                let nested_items = get_bookmarks_group_items(
                    &self.state.bookmarks,
                    self.state.project_bookmarks.as_ref(),
                    group_name,
                    self.state.ui.current_bookmarks_group_is_project,
                );
                if !nested_items.is_empty() {
                    let menu_x = get_menu_item_x_position(BOOKMARKS_MENU_INDEX);
                    let parent_width = dropdown_width(&bookmarks_items);
                    let nested_x = menu_x + parent_width;
                    let nested_y = 2 + self.state.ui.bookmarks_submenu.selected as u16;
                    if let Some(index) = hit_dropdown_item(x, y, nested_x, nested_y, &nested_items)
                    {
                        self.state.ui.bookmarks_nested.selected = index;
                        self.execute_bookmarks_nested_action()?;
                        return Ok(true);
                    }
                }
            }
        }

        // Check click on Bookmarks main dropdown
        let menu_x = get_menu_item_x_position(BOOKMARKS_MENU_INDEX);
        if let Some(index) = hit_dropdown_item(x, y, menu_x, 1, &bookmarks_items) {
            self.state.ui.bookmarks_submenu.selected = index;
            self.execute_bookmarks_submenu_action()?;
            return Ok(true);
        }

        self.state.close_menu();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termide_ui_render::DropdownItem;

    // Regression: `hit_dropdown_item` sized dropdowns as `label + 4`, while
    // `Dropdown` draws them `label + shortcut column + 6` wide. Clicks on the
    // right part of a drawn dropdown fell outside it, and nested submenus
    // (placed at `menu_x + drawn width`) were hit-tested at the wrong column.

    #[test]
    fn click_on_shortcut_column_selects_the_row() {
        let items = vec![
            DropdownItem::new("Open", "open"),
            DropdownItem::new("Terminal", "terminal")
                .with_submenu()
                .with_shortcut(Some("Alt+T".into())),
            DropdownItem::new("Files", "files").with_shortcut(Some("Alt+F".into())),
        ];
        let width = dropdown_width(&items);

        // On the "Alt+F" text of the "Files" row (index 2, below the border).
        assert_eq!(hit_dropdown_item(width - 4, 3, 0, 0, &items), Some(2));
        // One column past the right border.
        assert_eq!(hit_dropdown_item(width, 3, 0, 0, &items), None);
    }

    #[test]
    fn click_near_right_border_of_nested_submenu_selects_the_row() {
        let shells = vec![
            DropdownItem::new("Sh", "/bin/sh"),
            DropdownItem::new("Bash", "/bin/bash"),
        ];
        let (x0, y0) = (20, 3);
        let width = dropdown_width(&shells);

        // Last column inside the right border, on the "Bash" row.
        assert_eq!(
            hit_dropdown_item(x0 + width - 2, y0 + 2, x0, y0, &shells),
            Some(1)
        );
    }
}
