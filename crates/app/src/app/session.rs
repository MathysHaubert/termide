//! Session management operations.
//!
//! Contains save/load session functionality for persisting application state.

use anyhow::Result;
use termide_layout::LayoutManager;
use termide_panel_editor::Editor;

use crate::LayoutManagerSession;
use crate::PanelExt;

use super::App;

impl App {
    /// Save current session to file
    pub(super) fn save_session(&mut self) -> Result<()> {
        // In $EDITOR mode (launched with file arguments) the session is not
        // persisted, so editing a commit message or crontab never clobbers the
        // project's real session.
        if !self.persist_session {
            return Ok(());
        }

        // Get session directory for this project
        let session_dir = termide_project::Session::get_project_dir(&self.project_root)?;

        // Ensure all modified unnamed buffers have stable filenames
        for group in &mut self.layout_manager.panel_groups {
            for panel in group.panels_mut() {
                if let Some(editor) = panel.as_editor_mut() {
                    editor.ensure_unsaved_buffer_file();
                }
            }
        }

        // Serialize layout to session (may save temporary buffers)
        let session = self.layout_manager.to_state(&session_dir);

        // Save session to file
        session.save(&self.project_root)?;

        // Remove stale unsaved buffer files not referenced by current session
        termide_project::cleanup_stale_buffers(&session_dir, &session);

        log::info!("Session saved");
        Ok(())
    }

    /// Load session from file and restore layout
    pub fn load_session(&mut self) -> Result<()> {
        // Load session for this project
        let session = termide_project::Session::load(&self.project_root)?;

        // Get session directory for restoring temporary buffers
        let session_dir = termide_project::Session::get_project_dir(&self.project_root)?;

        // Get terminal dimensions for creating Terminal panels
        // Height: subtract menu (1) + status bar (1) + panel border (1) = 3
        let term_height = self.state.terminal.height.saturating_sub(3);
        // Width: full terminal width (vertical layout doesn't reduce width)
        let term_width = self.state.terminal.width;

        // Restore layout from session
        self.layout_manager = LayoutManager::from_session(
            session,
            &session_dir,
            term_height,
            term_width,
            self.state.editor_config(),
        )?;

        // Adapt panel widths to current terminal size
        self.layout_manager
            .redistribute_widths_proportionally(term_width);

        log::info!("Session loaded");

        // Register watchers for the new panels
        self.state.needs_watcher_registration = true;

        // Initialize LSP for all restored editors
        if let Some(ref mut lsp_manager) = self.state.lsp_manager {
            for group in &mut self.layout_manager.panel_groups {
                for panel in group.panels_mut() {
                    if let Some(editor) = panel.as_editor_mut() {
                        editor.init_lsp(lsp_manager);
                    }
                }
            }
        }

        // Restore orphaned buffer files (not referenced in session anymore).
        // The recovery itself is automatic — orphans appear as new
        // editor panels so user data from a crashed session isn't lost
        // — but it used to be silent, so users seeing extra editor
        // tabs had no way to tell what they were. Surface a Journal
        // entry per restored buffer plus a single summary so the
        // information is one panel open away.
        match termide_project::restore_orphaned_buffers(&session_dir) {
            Ok(orphaned_files) => {
                let mut restored = 0usize;
                for buffer_file in orphaned_files {
                    if let Ok(content) =
                        termide_project::load_unsaved_buffer(&session_dir, &buffer_file)
                    {
                        let mut editor = Editor::with_config(self.state.editor_config());
                        if editor.insert_text(&content).is_ok() {
                            editor.set_unsaved_buffer_file(Some(buffer_file.clone()));
                            self.add_panel(Box::new(editor));
                            log::info!(
                                "Recovered unsaved buffer from previous session: {}",
                                buffer_file
                            );
                            restored += 1;
                        }
                    }
                }
                if restored > 0 {
                    log::warn!(
                        "Restored {restored} unsaved buffer(s) from a previous session — \
                         they appear as new editor panels."
                    );
                }
            }
            Err(e) => log::warn!("Failed to restore orphaned buffers: {}", e),
        }

        Ok(())
    }

    /// Auto-save session (ignores errors to not disrupt user experience)
    pub fn auto_save_session(&mut self) {
        if let Err(e) = self.save_session() {
            // Log error but don't interrupt user workflow
            log::error!("Failed to auto-save session: {}", e);
        }
    }
}
