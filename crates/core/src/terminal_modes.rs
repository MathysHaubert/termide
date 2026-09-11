//! Entering and leaving the terminal modes termide runs in.
//!
//! These live here rather than in `main` because a detached session has to
//! re-enter them: a client that attaches later is looking at a terminal that
//! knows nothing of the alternate screen, mouse reporting or bracketed paste
//! the hosted process switched on when it started. Startup and reattach must
//! agree exactly, which is easiest to guarantee when they are the same code.

use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use std::io;

use termide_keyboard::KeyboardCaps;

/// Switch the terminal into the modes the UI assumes.
///
/// Safe to call more than once: every sequence here is idempotent from the
/// terminal's point of view, except the keyboard-enhancement push, which
/// stacks. That only matters when reattaching to the *same* terminal twice,
/// where one extra stack entry is harmless and is popped on exit anyway.
pub fn enter_terminal_modes(caps: &KeyboardCaps, title: Option<&str>) -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange,
        EnableBracketedPaste
    )?;

    if let Some(title) = title {
        execute!(stdout, SetTitle(title))?;
    }

    if caps.kitty_full {
        // REPORT_EVENT_TYPES exposes `KeyEventState::CAPS_LOCK` on every key
        // event, which the hotkey matcher uses to ignore the spurious Shift
        // modifier that Caps Lock attaches to letters.
        //
        // REPORT_ALTERNATE_KEYS is what makes shifted characters typable under
        // REPORT_ALL_KEYS_AS_ESCAPE_CODES, where the protocol's primary
        // codepoint is the key *without* modifiers.
        let mut flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES;

        // REPORT_ALL_KEYS_AS_ESCAPE_CODES is what makes macOS `Option+<letter>`
        // reach us as `Alt+<letter>`. `KeyboardCaps::detect` restricts it to
        // macOS and honours `general.report_all_keys`, because it also stops
        // dead-key and IME composition from reaching the app.
        if caps.all_keys {
            flags |= KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        }

        execute!(stdout, PushKeyboardEnhancementFlags(flags))?;
    }

    Ok(())
}

/// Put the terminal back the way it was found.
///
/// Used on exit, from the panic hook, and by the attach client when its
/// connection ends — the client never enabled these itself, but it is the one
/// holding the terminal when a session lets go of it.
pub fn leave_terminal_modes() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste,
        SetTitle("")
    );
    let _ = execute!(stdout, crossterm::cursor::Show);
}
