//! The fork must agree with terminals on grapheme-cluster widths: ratatui's
//! frame diff skips cells based on these numbers, and a mismatch shifts the
//! rest of the row on screen (the "leftover border fragments" bug).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[test]
fn variation_selector_width_follows_the_host_terminal() {
    // The whole toggle lives in this one test: the switch is process-global
    // and `cargo test` runs the tests of a binary on parallel threads.
    let vs16 = [
        "\u{2714}\u{FE0F}",
        "\u{26A0}\u{FE0F}",
        "\u{23F1}\u{FE0F}",
        "\u{270D}\u{FE0F}",
    ];

    // Default: upstream UAX #11 semantics (Ghostty, WezTerm, iTerm2) — VS16
    // makes a text-presentation emoji wide, VS15 makes a wide one narrow.
    assert!(unicode_width::variation_selectors_change_width());
    for s in vs16 {
        assert_eq!(s.width(), 2, "{s:?}");
    }
    assert_eq!("\u{26A1}\u{FE0E}".width(), 1);
    assert_eq!("\u{2714}".width(), 1);
    assert_eq!("\u{2705}".width(), 2);

    // Host keeps wcwidth (alacritty, foot): the selector only changes the glyph.
    unicode_width::set_variation_selectors_change_width(false);
    for s in vs16 {
        assert_eq!(s.width(), 1, "{s:?}");
    }
    assert_eq!("\u{26A1}\u{FE0E}".width(), 2);
    unicode_width::set_variation_selectors_change_width(true);
}

#[test]
fn zwj_and_modifier_sequences_take_one_cell_pair() {
    assert_eq!("👨\u{200D}👩\u{200D}👧".width(), 2);
    assert_eq!("👍\u{1F3FD}".width(), 2);
    assert_eq!("🇷🇺".width(), 2);
    assert_eq!("🔥".width(), 2);
}

#[test]
fn plain_characters_keep_uax11_widths() {
    assert_eq!("中文".width(), 4);
    assert_eq!("e\u{0301}".width(), 1);
    assert_eq!("─│┌┐".width(), 4);
    assert_eq!("Привет".width(), 6);
    assert_eq!('\u{200B}'.width(), Some(0));
    assert_eq!('\u{0007}'.width(), None);
    assert_eq!(' '.width(), Some(1));
}

#[test]
fn bengali_spacing_marks_follow_glibc() {
    for c in [
        '\u{09BE}', '\u{09BF}', '\u{09C0}', '\u{09C7}', '\u{09C8}', '\u{09CB}', '\u{09CC}',
        '\u{09D7}',
    ] {
        assert_eq!(c.width(), Some(1), "U+{:04X}", c as u32);
        assert_eq!(c.width_cjk(), Some(1), "U+{:04X}", c as u32);
    }
    // ka + aa: both advance the cursor.
    assert_eq!("\u{0995}\u{09BE}".width(), 2);
}
