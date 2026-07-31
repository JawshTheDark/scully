// Finding the next buffer that wants you.
//
// Pure index arithmetic over a snapshot of the sidebar, so the "which buffer
// next?" rule is unit-tested rather than discovered by clicking around. The
// window supplies the rows in display order and reads the answer back.

/// One buffer's claim on your attention, as the sidebar knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activity {
    pub unread: i64,
    pub highlights: i64,
    /// Header placeholders occupy a row index but can never be selected.
    pub selectable: bool,
}

impl Activity {
    pub fn none() -> Self {
        Self { unread: 0, highlights: 0, selectable: true }
    }
    fn wants_attention(&self) -> bool {
        self.selectable && (self.unread > 0 || self.highlights > 0)
    }
}

/// The next row worth visiting after `from`, searching forward and wrapping.
///
/// Highlights outrank plain unread: a direct mention is the thing you actually
/// need, and letting a chatty channel bury it is how people miss being spoken
/// to. So the whole list is checked for highlights first, and only if there are
/// none does plain unread decide.
///
/// `from` is the current row (`None` when nothing is selected). Returns `None`
/// when nothing anywhere is unread — including when the *only* unread buffer is
/// the one you are already reading, since jumping to where you already are
/// would look like the key did nothing.
pub fn next_attention(rows: &[Activity], from: Option<usize>) -> Option<usize> {
    let pick = |want_highlight: bool| {
        let n = rows.len();
        if n == 0 {
            return None;
        }
        let start = from.map(|i| i + 1).unwrap_or(0);
        (0..n)
            .map(|off| (start + off) % n)
            .filter(|&i| Some(i) != from)
            .find(|&i| {
                let a = rows[i];
                a.wants_attention() && (!want_highlight || a.highlights > 0)
            })
    };
    pick(true).or_else(|| pick(false))
}

/// The row index for a 1-based position among the *selectable* rows, as typed
/// with Alt+1…Alt+9. Header placeholders are skipped, so "3" means the third
/// buffer a person can see, not the third widget in the list.
pub fn nth_selectable(rows: &[Activity], nth: usize) -> Option<usize> {
    if nth == 0 {
        return None;
    }
    rows.iter()
        .enumerate()
        .filter(|(_, a)| a.selectable)
        .map(|(i, _)| i)
        .nth(nth - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(unread: i64, highlights: i64) -> Activity {
        Activity { unread, highlights, selectable: true }
    }
    fn header() -> Activity {
        Activity { unread: 0, highlights: 0, selectable: false }
    }
    fn quiet() -> Activity {
        Activity::none()
    }

    #[test]
    fn a_highlight_outranks_unread_even_when_it_is_further_away() {
        // Row 1 has chatter, row 3 has your name in it. Being spoken to wins,
        // regardless of which comes first in the list.
        let rows = [quiet(), row(50, 0), quiet(), row(1, 1)];
        assert_eq!(next_attention(&rows, Some(0)), Some(3));
    }

    #[test]
    fn plain_unread_is_used_when_nothing_is_highlighted() {
        let rows = [quiet(), row(3, 0), quiet()];
        assert_eq!(next_attention(&rows, Some(0)), Some(1));
    }

    #[test]
    fn the_search_wraps_around_the_end() {
        let rows = [row(2, 0), quiet(), quiet()];
        assert_eq!(next_attention(&rows, Some(2)), Some(0), "wraps to the top");
    }

    #[test]
    fn the_row_you_are_on_is_never_the_answer() {
        // Otherwise the key appears to do nothing while you sit on the only
        // unread buffer.
        let rows = [row(5, 2), quiet()];
        assert_eq!(next_attention(&rows, Some(0)), None);
    }

    #[test]
    fn nothing_unread_anywhere_is_none() {
        assert_eq!(next_attention(&[quiet(), quiet()], Some(0)), None);
        assert_eq!(next_attention(&[], None), None);
    }

    #[test]
    fn headers_are_never_selected() {
        // A header placeholder can carry no activity, but pin it anyway: it
        // shares the row indices with real buffers.
        let rows = [header(), row(1, 0)];
        assert_eq!(next_attention(&rows, None), Some(1));
        let mut noisy = header();
        noisy.unread = 99;
        assert_eq!(next_attention(&[noisy, quiet()], None), None, "header cannot be picked");
    }

    #[test]
    fn starting_from_nothing_scans_from_the_top() {
        let rows = [quiet(), row(1, 0)];
        assert_eq!(next_attention(&rows, None), Some(1));
    }

    #[test]
    fn alt_number_counts_buffers_not_widgets() {
        // [header, A, header, B, C] — "2" means B, the second *buffer*.
        let rows = [header(), row(0, 0), header(), row(0, 0), row(0, 0)];
        assert_eq!(nth_selectable(&rows, 1), Some(1));
        assert_eq!(nth_selectable(&rows, 2), Some(3));
        assert_eq!(nth_selectable(&rows, 3), Some(4));
        assert_eq!(nth_selectable(&rows, 4), None, "past the end");
        assert_eq!(nth_selectable(&rows, 0), None, "there is no zeroth buffer");
    }
}
