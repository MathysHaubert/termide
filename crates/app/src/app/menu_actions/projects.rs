//! Sessions menu actions — session switching, directory switcher.

use anyhow::Result;
use std::path::PathBuf;

use super::super::App;
use crate::projects_menu::ProjectsTarget;
use crate::state::{ActiveModal, PendingAction};
use crate::PanelExt;
use termide_app_core::Panel;
use termide_i18n as i18n;
use termide_ui_render::{
    PROJECTS_SUBMENU_CHANGE_ROOT, PROJECTS_SUBMENU_NEW, PROJECTS_SUBMENU_SWITCH,
};

impl App {
    /// Open sessions modal to switch between projects
    pub(in crate::app) fn handle_open_projects_modal(&mut self) -> Result<()> {
        use termide_modal::{ProjectsModal, SessionItem};
        use termide_project::{format_relative_time, list_all_projects};

        let t = i18n::t();

        // Get all sessions
        let sessions = list_all_projects().unwrap_or_default();

        // Get current project path
        let current_project = std::env::current_dir().unwrap_or_default();

        // Convert to SessionItems
        let items: Vec<SessionItem> = sessions
            .into_iter()
            .map(|info| {
                let is_current = info.project_path == current_project;
                let display_path =
                    termide_core::util::shorten_home_path(&info.project_path.display().to_string());
                let relative_time = format_relative_time(info.modified);

                SessionItem {
                    project_path: info.project_path,
                    display_path,
                    relative_time,
                    is_current,
                }
            })
            .collect();

        // Only show modal if there are other sessions
        if items.iter().any(|item| !item.is_current) {
            // Find index of current session to position cursor there
            let current_idx = items.iter().position(|item| item.is_current).unwrap_or(0);
            let modal = ProjectsModal::new(t.projects_title(), items).with_cursor(current_idx);
            self.state.set_pending_action(
                PendingAction::SwitchSession,
                ActiveModal::Sessions(Box::new(modal)),
            );
        }

        Ok(())
    }

    /// Open directory switcher modal
    pub(in crate::app) fn handle_open_directory_switcher(&mut self) -> Result<()> {
        use termide_modal::{DirectoryItem, DirectorySwitcherModal};

        let t = i18n::t();

        // Check if active panel supports directory switching (Terminal or FileManager)
        let panel_supported = self
            .layout_manager
            .active_panel_mut()
            .map(|p| p.as_terminal_mut().is_some() || p.as_file_manager_mut().is_some())
            .unwrap_or(false);

        if !panel_supported {
            self.state
                .set_info(t.directory_switcher_unsupported().to_string());
            return Ok(());
        }

        // For terminal panels, check if there's a running process (cd won't work)
        let has_running_process = self
            .layout_manager
            .active_panel_mut()
            .and_then(|p| p.as_terminal_mut())
            .map(|t| t.has_running_processes())
            .unwrap_or(false);

        if has_running_process {
            self.state
                .set_info(t.directory_switcher_process_running().to_string());
            return Ok(());
        }

        // Get current panel's working directory
        let current_dir = self
            .layout_manager
            .active_panel_mut()
            .and_then(|p| p.get_working_directory());

        // Get all unique paths from all panels
        let panel_paths = self.collect_panel_paths();

        // Get bookmarked directories
        let bookmark_dirs = self.state.bookmarks.directories();

        // Build combined items list
        let mut items: Vec<DirectoryItem> = Vec::new();
        let mut seen_paths = std::collections::HashSet::new();

        // Add panel paths first
        for path in panel_paths {
            let is_current = current_dir.as_ref() == Some(&path);
            let display = termide_core::util::shorten_home_path(&path.display().to_string());
            seen_paths.insert(path.clone());
            items.push(DirectoryItem {
                path,
                display,
                is_current,
                is_bookmark: false,
            });
        }

        // Add bookmarked directories (if not already in list)
        for bookmark in bookmark_dirs {
            let path = PathBuf::from(&bookmark.path);
            if !seen_paths.contains(&path) {
                // Show path instead of display name for consistency
                let display = termide_core::util::shorten_home_path(&bookmark.path);
                let is_current = current_dir.as_ref() == Some(&path);
                items.push(DirectoryItem {
                    path,
                    display,
                    is_current,
                    is_bookmark: true,
                });
            }
        }

        // Drive roots (Windows only): the one way to another drive without
        // typing its path, and what ".." at a drive root opens.
        for path in termide_vfs::drive_roots() {
            if items.iter().any(|item| item.path == path) {
                continue;
            }
            items.push(DirectoryItem {
                display: path.display().to_string(),
                is_current: current_dir.as_ref() == Some(&path),
                path,
                is_bookmark: false,
            });
        }

        // Sort items alphabetically by display path
        items.sort_by(|a, b| a.display.cmp(&b.display));

        // If no paths available, show info message
        if items.is_empty() {
            self.state
                .set_info(t.directory_switcher_no_paths().to_string());
            return Ok(());
        }

        // Find index of current directory to position cursor there
        let current_idx = items.iter().position(|item| item.is_current).unwrap_or(0);
        let modal = DirectorySwitcherModal::new(t.directory_switcher_title(), items)
            .with_cursor(current_idx);
        self.state.set_pending_action(
            PendingAction::SwitchDirectory,
            ActiveModal::DirectorySwitcher(Box::new(modal)),
        );

        Ok(())
    }

    // =========================================================================
    // Sessions submenu handling
    // =========================================================================

    /// Handle keyboard event in the Projects menu. Keys act on the deepest
    /// open level: Right/Enter open a directory, Left/Esc close it again.
    pub(in crate::app) fn handle_projects_submenu_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<()> {
        use super::navigate_submenu;
        use super::SubmenuNavAction;

        let levels = self.state.projects_menu_levels(self.screen_rect());
        let depth = levels.len() - 1;
        let level = &levels[depth];
        let target = ProjectsTarget::of(level.selected_row());
        let mut cursor = termide_state::SubmenuState {
            open: true,
            selected: level.selected,
        };
        let action = navigate_submenu(&key, &mut cursor, level.items.len(), &level.separators());
        drop(levels);
        self.select_projects_row(depth, cursor.selected);

        match action {
            SubmenuNavAction::Close if depth > 0 => {
                self.state.ui.projects_nested.pop();
            }
            SubmenuNavAction::Close => self.state.close_menu(),
            SubmenuNavAction::Left if depth > 0 => {
                self.state.ui.projects_nested.pop();
            }
            SubmenuNavAction::Left => self.switch_to_prev_menu()?,
            SubmenuNavAction::Right if matches!(target, ProjectsTarget::Submenu) => {
                self.state.ui.projects_nested.push(0);
            }
            SubmenuNavAction::Right => self.switch_to_next_menu()?,
            SubmenuNavAction::Execute => self.activate_projects_target(target, false)?,
            SubmenuNavAction::Rename
            | SubmenuNavAction::Edit
            | SubmenuNavAction::Delete
            | SubmenuNavAction::None => {}
        }
        Ok(())
    }

    /// Select `index` at menu level `depth` and close every level below it.
    pub(in crate::app) fn select_projects_row(&mut self, depth: usize, index: usize) {
        let ui = &mut self.state.ui;
        ui.projects_nested.truncate(depth);
        match depth.checked_sub(1) {
            None => ui.projects_submenu.selected = index,
            Some(parent) => ui.projects_nested[parent] = index,
        }
    }

    /// Carry out the selected row. `was_open` is whether the row's submenu
    /// was open before it was selected, so a second click folds it again.
    pub(in crate::app) fn activate_projects_target(
        &mut self,
        target: ProjectsTarget,
        was_open: bool,
    ) -> Result<()> {
        match target {
            ProjectsTarget::Submenu if !was_open => self.state.ui.projects_nested.push(0),
            ProjectsTarget::Submenu | ProjectsTarget::None => {}
            ProjectsTarget::Project(path) => {
                self.state.close_menu();
                if path != self.project_root {
                    self.switch_to_session(path)?;
                }
            }
            ProjectsTarget::Action(PROJECTS_SUBMENU_NEW) => {
                self.state.close_menu();
                self.handle_new_project()?;
            }
            ProjectsTarget::Action(PROJECTS_SUBMENU_SWITCH) => {
                self.state.close_menu();
                self.handle_open_projects_modal()?;
            }
            ProjectsTarget::Action(PROJECTS_SUBMENU_CHANGE_ROOT) => {
                self.state.close_menu();
                self.handle_change_root_path()?;
            }
            ProjectsTarget::Action(_) => {}
        }
        Ok(())
    }

    /// Open directory picker for creating new session
    pub(in crate::app) fn handle_new_project(&mut self) -> Result<()> {
        use termide_modal::DirectoryPickerModal;

        let t = i18n::t();
        // Get current project root as starting directory
        let initial_dir = self.project_root.clone();

        let modal = DirectoryPickerModal::new(
            initial_dir,
            t.projects_new().to_string(),
            t.directory_picker_create().to_string(),
        );
        self.state.set_pending_action(
            PendingAction::NewSession,
            ActiveModal::DirectoryPicker(Box::new(modal)),
        );

        Ok(())
    }

    /// Open directory picker for changing root path of current session
    pub(in crate::app) fn handle_change_root_path(&mut self) -> Result<()> {
        use termide_modal::DirectoryPickerModal;

        let t = i18n::t();
        // Get current project root as starting directory
        let initial_dir = self.project_root.clone();

        let modal = DirectoryPickerModal::new(
            initial_dir,
            t.projects_change_root().to_string(),
            t.directory_picker_move().to_string(),
        );
        self.state.set_pending_action(
            PendingAction::ChangeRootPath,
            ActiveModal::DirectoryPicker(Box::new(modal)),
        );

        Ok(())
    }
}
