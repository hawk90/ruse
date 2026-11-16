//! Parsing the keys typed after `zf` into a linewise fold motion.

use ruse_core::Motion;

/// The parse result of feeding the keys typed after `zf` (Normal-mode fold-over-motion): the buffered
/// key string (e.g. `"3j"`, `"gg"`, `"ip"`) resolves to a linewise [`Motion`] + count for the
/// `reindent_range` seam, needs another key, or is not a supported fold motion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FoldMotionParse {
    /// A complete `{count}{motion}` — feed to `Workspace::reindent_range` to get the fold's line range.
    Ready { motion: Motion, count: u32 },
    /// A prefix so far (a partial count, or a multi-key motion lead like `g`/`i`/`a`) — await more keys.
    More,
    /// Not a fold motion — abandon the pending `zf`.
    Cancel,
}

/// Parse the accumulated `zf{motion}` key string into a linewise motion + count for the reindent seam.
/// Recognizes the fold-relevant motions (`:help zf` over the common set): `j`/`k` (line down/up), `G`
/// (to EOF, or `{count}G` to a line), `gg` (to a line), `}`/`{` (paragraph fwd/back), `ip`/`ap`
/// (inner/around paragraph). A leading `1`-`9` run is the count; a bare/partial prefix returns `More`.
#[must_use]
pub fn parse_fold_motion(keys: &str) -> FoldMotionParse {
    // A leading `1`-`9`… run is the count (a leading `0` is NOT a count — it is the column-0 motion, which
    // is not a supported fold motion, so it falls through to `Cancel`).
    let digit_len = if keys.starts_with(|c: char| ('1'..='9').contains(&c)) {
        keys.chars().take_while(char::is_ascii_digit).count()
    } else {
        0
    };
    let count = keys[..digit_len].parse::<u32>().unwrap_or(1).max(1);
    let rest = &keys[digit_len..];
    if rest.is_empty() {
        return FoldMotionParse::More; // still typing the count
    }
    let motion = match rest {
        "j" => Motion::Down,
        "k" => Motion::Up,
        // `{count}G` targets a line (`GotoLine`); bare `G` is to the last line.
        "G" => {
            if digit_len > 0 {
                Motion::GotoLine
            } else {
                Motion::LastLine
            }
        }
        "}" => Motion::ParagraphFwd,
        "{" => Motion::ParagraphBack,
        // Multi-key leads: await the second key.
        "g" | "i" | "a" => return FoldMotionParse::More,
        "gg" => Motion::GotoLine,
        // A move within the line folds the cursor line alone, whatever its count (`zfl`, `zf$`).
        "l" | "h" | "$" | "0" | "^" | " " => {
            return FoldMotionParse::Ready {
                motion: Motion::Line,
                count: 1,
            }
        }
        "_" => Motion::Line,
        "ip" => Motion::InnerParagraph,
        "ap" => Motion::AParagraph,
        _ => return FoldMotionParse::Cancel,
    };
    FoldMotionParse::Ready { motion, count }
}

#[cfg(test)]
mod tests;
