// Client-side estimate of how many IRC lines a message splits into, ported
// from the web client (`vue_client/src/utils/messageSplit.ts`) so the two
// clients gate the same message the same way. The actual splitting happens
// server-side via irc-framework; this is guidance for the confirm gate
// (`chat.allow_split_messages`), not a wire-level decision.

/// Mirrors the server's message_max_length default. Keep in sync with
/// `MESSAGE_MAX_BYTES` in the web client.
pub const MESSAGE_MAX_BYTES: usize = 350;
/// `ACTION` + \x01…\x01 framing overhead.
pub const ACTION_MAX_BYTES: usize = MESSAGE_MAX_BYTES - ("ACTION".len() + 3);

/// Greedy word-pack over whitespace-separated tokens, byte-aware. A single
/// token over budget is counted in budget-sized code-point slices — close
/// enough to irc-framework's cascade for anything a human types.
pub fn chunk_count(body: &str, budget: usize) -> usize {
    if body.is_empty() || budget == 0 {
        return if body.is_empty() { 0 } else { 1 };
    }
    // Each newline-separated line splits independently.
    body.lines().map(|l| chunks_for_line(l, budget)).sum::<usize>().max(1)
}

fn chunks_for_line(line: &str, budget: usize) -> usize {
    if line.is_empty() {
        return 0;
    }
    if line.len() <= budget {
        return 1;
    }
    let mut count = 0usize;
    let mut cur = String::new();
    let mut pending_ws = String::new();

    let flush_new = |cur: &mut String, count: &mut usize, word: &str| {
        if !cur.is_empty() {
            *count += 1;
            cur.clear();
        }
        if word.len() <= budget {
            cur.push_str(word);
            return;
        }
        let mut acc = String::new();
        for cp in word.chars() {
            if acc.len() + cp.len_utf8() > budget {
                *count += 1;
                acc.clear();
            }
            acc.push(cp);
        }
        *cur = acc;
    };

    // Tokenize preserving the whitespace runs between words, like the JS
    // `split(/(\s+)/)`.
    let mut tokens: Vec<(bool, &str)> = Vec::new();
    let mut start = 0;
    let mut in_ws = None::<bool>;
    for (i, ch) in line.char_indices() {
        let ws = ch.is_whitespace();
        match in_ws {
            Some(prev) if prev == ws => {}
            _ => {
                if i > start {
                    tokens.push((in_ws.unwrap_or(ws), &line[start..i]));
                }
                start = i;
                in_ws = Some(ws);
            }
        }
    }
    if start < line.len() {
        tokens.push((in_ws.unwrap_or(false), &line[start..]));
    }

    for (is_ws, tok) in tokens {
        if is_ws {
            pending_ws = tok.to_string();
            continue;
        }
        if cur.is_empty() {
            flush_new(&mut cur, &mut count, tok);
            continue;
        }
        if cur.len() + pending_ws.len() + tok.len() <= budget {
            cur.push_str(&pending_ws);
            cur.push_str(tok);
            pending_ws.clear();
        } else {
            flush_new(&mut cur, &mut count, tok);
        }
    }
    if !cur.is_empty() {
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_messages_are_one_chunk() {
        assert_eq!(chunk_count("hello world", MESSAGE_MAX_BYTES), 1);
        assert_eq!(chunk_count("", MESSAGE_MAX_BYTES), 0);
    }

    #[test]
    fn long_messages_split_word_greedily() {
        let msg = "word ".repeat(100); // 500 bytes
        assert_eq!(chunk_count(msg.trim(), MESSAGE_MAX_BYTES), 2);
    }

    #[test]
    fn a_single_oversized_token_is_sliced_by_bytes() {
        let blob = "x".repeat(800);
        assert_eq!(chunk_count(&blob, MESSAGE_MAX_BYTES), 3);
    }

    #[test]
    fn multibyte_slicing_never_tears_a_code_point() {
        // é is 2 bytes; a budget of 3 fits one é per slice with no panic from
        // slicing mid-sequence.
        let s = "ééééé";
        assert_eq!(chunk_count(s, 4), 3);
    }

    #[test]
    fn action_budget_is_smaller() {
        assert!(ACTION_MAX_BYTES < MESSAGE_MAX_BYTES);
        let msg = "a".repeat(MESSAGE_MAX_BYTES);
        assert_eq!(chunk_count(&msg, ACTION_MAX_BYTES), 2);
    }
}
