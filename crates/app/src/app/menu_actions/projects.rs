//! Sessions menu actions — session switching, directory switcher.

use anyhow::Result;
use std::path::PathBuf;

use super::super::App;
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

    /// Handle keyboard event in Sessions submenu
    pub(in crate::app) fn handle_projects_submenu_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<()> {
        use super::navigate_submenu;
        use super::SubmenuNavAction;
        use termide_ui_render::PROJECTS_SUBMENU_ITEM_COUNT;

        match navigate_submenu(
            &key,
            &mut self.state.ui.projects_submenu,
            PROJECTS_SUBMENU_ITEM_COUNT,
            &[],
        ) {
            SubmenuNavAction::Close => self.state.close_menu(),
            SubmenuNavAction::Execute => self.execute_projects_submenu_action()?,
            SubmenuNavAction::Right => self.switch_to_next_menu()?,
            SubmenuNavAction::Left => self.switch_to_prev_menu()?,
            SubmenuNavAction::Rename
            | SubmenuNavAction::Edit
            | SubmenuNavAction::Delete
            | SubmenuNavAction::None => {}
        }
        Ok(())
    }

    /// Execute action for selected Sessions submenu item
    pub(in crate::app) fn execute_projects_submenu_action(&mut self) -> Result<()> {
        match self.state.ui.projects_submenu.selected {
            PROJECTS_SUBMENU_NEW => {
                self.state.close_menu();
                self.handle_new_project()?;
            }
            PROJECTS_SUBMENU_SWITCH => {
                self.state.close_menu();
                self.handle_open_projects_modal()?;
            }
            PROJECTS_SUBMENU_CHANGE_ROOT => {
                self.state.close_menu();
                self.handle_change_root_path()?;
            }
            _ => {}
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
