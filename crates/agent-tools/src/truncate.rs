//! Output limits shared by the tools.

/// Default number of lines `read` returns per call.
pub const READ_MAX_LINES: usize = 2000;
/// Byte budget of one `read` result, whichever limit is hit first.
pub const READ_MAX_BYTES: usize = 64 * 1024;
/// Byte budget of shell output kept inline; the rest goes to a file.
pub const SHELL_MAX_BYTES: usize = 48 * 1024;

/// Split `text` into a head and a tail that together fit `max_bytes`, cut on
/// line boundaries with the head taking about 60% of the budget. Returns
/// `None` when the text already fits. The middle part is what the caller
/// replaces with an omission marker.
#[must_use]
pub fn head_tail(text: &str, max_bytes: usize) -> Option<HeadTail<'_>> {
    if text.len() <= max_bytes {
        return None;
    }
    let head_budget = max_bytes * 6 / 10;
    let tail_budget = max_bytes - head_budget;

    let mut head_end = 0;
    for (index, _) in text.match_indices('\n') {
        if index + 1 > head_budget {
            break;
        }
        head_end = index + 1;
    }

    let mut tail_start = text.len();
    for (index, _) in text.rmatch_indices('\n') {
        if text.len() - (index + 1) > tail_budget {
            break;
        }
        tail_start = index + 1;
    }
    if tail_start <= head_end {
        return None;
    }

    let omitted_lines = text[head_end..tail_start].matches('\n').count();
    Some(HeadTail {
        head: &text[..head_end],
        tail: &text[tail_start..],
        omitted_lines,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub struct HeadTail<'a> {
    pub head: &'a str,
    pub tail: &'a str,
    pub omitted_lines: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_not_truncated() {
        assert!(head_tail("a\nb\n", 100).is_none());
    }

    #[test]
    fn head_and_tail_cut_on_line_boundaries() {
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let cut = head_tail(&text, 200).expect("truncated");
        assert!(cut.head.starts_with("line 1\n"));
        assert!(cut.head.ends_with('\n'));
        assert!(cut.tail.ends_with("line 100\n"));
        assert!(cut.tail.starts_with("line "));
        assert!(cut.head.len() + cut.tail.len() <= 200);
        assert!(cut.omitted_lines > 50);
    }
}
