//! Common input key handling for modal text fields.
//!
//! A thin adapter over [`termide_ui::field_edit`], the grammar every text field
//! in the app shares — the input bars, the agent's prompt box and the modal
//! inputs alike — re-expressed as the [`InputKeyResult`] the modals act on.
//! See that module for the keys themselves; `Enter`, `Tab` and `Esc` belong to
//! the modal and are deliberately left to it.

use crossterm::event::KeyEvent;

use crate::TextInputHandler;

/// Result of input key handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKeyResult {
    /// Key was handled by input handler.
    Handled,
    /// Key was not handled - should be processed by modal.
    NotHandled,
    /// Text was modified (for modals that need to react to changes).
    TextModified,
}

/// Handle common text input keys: navigation, selection, clipboard and
/// undo/redo.
///
/// Returns `InputKeyResult::TextModified` when the text changed,
/// `InputKeyResult::Handled` when only the cursor or the selection moved (or
/// text went to the clipboard), `InputKeyResult::NotHandled` when the modal
/// should process the key itself.
pub fn handle_input_key(input: &mut TextInputHandler, key: KeyEvent) -> InputKeyResult {
    match termide_ui::edit_text_input(input, key) {
        termide_ui::FieldEdit::Edited => InputKeyResult::TextModified,
        termide_ui::FieldEdit::Navigated => InputKeyResult::Handled,
        termide_ui::FieldEdit::NotHandled => InputKeyResult::NotHandled,
    }
}
