//! The managed block: text between two marker lines that ccid owns inside a file a person owns.
//!
//! `render` replaces the block when it is there and inserts it when it is not; every other byte of the
//! file stays. Marker lines inside a fenced code block are not markers (a README may document them).

use crate::{failure, Result};

/// The first marker line starts with this; the rest of the line is a comment for the reader.
pub const BEGIN_PREFIX: &str = "<!-- ccid:security:begin";
/// The last marker line, exactly.
pub const END: &str = "<!-- ccid:security:end -->";

/// Where a block goes when the file has none yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// After the first level-1 heading (README); at the top of the body when there is none.
    AfterTitle,
    /// At the end of the file (SECURITY.md).
    End,
}

/// The file with its block applied, and whether the block was there before.
#[derive(Debug, PartialEq, Eq)]
pub struct Applied {
    pub text: String,
    pub had_block: bool,
}

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// A line without its terminator.
fn content(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|rest| rest.strip_suffix('\r').unwrap_or(rest))
        .unwrap_or(line)
}

/// Tracks fenced code blocks (``` or ~~~) line by line.
#[derive(Default)]
struct Fence {
    open: Option<char>,
}

impl Fence {
    /// True when `line` is inside a fence or is a fence line itself.
    fn inside(&mut self, line: &str) -> bool {
        let trimmed = line.trim_start();
        let marker = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        match (self.open, marker) {
            (None, Some(kind)) => {
                self.open = Some(kind);
                true
            }
            (Some(open), Some(kind)) if open == kind => {
                self.open = None;
                true
            }
            (Some(_), _) => true,
            (None, None) => false,
        }
    }
}

struct Markers {
    begins: Vec<usize>,
    /// The end of the end marker's text, before its line terminator.
    ends: Vec<usize>,
}

fn find_markers(text: &str) -> Markers {
    let mut markers = Markers {
        begins: Vec::new(),
        ends: Vec::new(),
    };
    let mut fence = Fence::default();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = content(line);
        if !fence.inside(body) {
            if body.starts_with(BEGIN_PREFIX) {
                markers.begins.push(offset);
            } else if body == END {
                markers.ends.push(offset + body.len());
            }
        }
        offset += line.len();
    }
    markers
}

/// Byte offset where the block goes for [`Place::AfterTitle`]: the end of the first `# ` heading line
/// outside front matter, fences and comments, else the top of the body.
fn after_title(text: &str) -> usize {
    let mut start = if text.starts_with('\u{feff}') { 3 } else { 0 };
    // YAML front matter.
    let rest = &text[start..];
    if rest.starts_with("---\n") || rest.starts_with("---\r\n") {
        let mut offset = start;
        for (index, line) in rest.split_inclusive('\n').enumerate() {
            offset += line.len();
            if index > 0 && content(line) == "---" {
                start = offset;
                break;
            }
        }
    }
    let mut fence = Fence::default();
    let mut comment = false;
    let mut offset = start;
    let mut leading = start;
    let mut still_leading = true;
    for line in text[start..].split_inclusive('\n') {
        let body = content(line);
        offset += line.len();
        if comment {
            if body.contains("-->") {
                comment = false;
            }
            continue;
        }
        if body.trim_start().starts_with("<!--") {
            if !body.contains("-->") {
                comment = true;
            } else if still_leading {
                leading = offset;
            }
            continue;
        }
        if fence.inside(body) {
            still_leading = false;
            continue;
        }
        if body.starts_with("# ") {
            return offset;
        }
        if body.trim().is_empty() {
            if still_leading {
                leading = offset;
            }
        } else {
            still_leading = false;
        }
    }
    leading
}

/// `existing` with `block` (marker lines included, `\n` line breaks, no trailing break) applied.
pub fn apply(existing: &str, block: &str, place: Place, name: &str) -> Result<Applied> {
    let eol = eol_of(existing);
    let block = block.replace('\n', eol);
    let markers = find_markers(existing);
    match (markers.begins.as_slice(), markers.ends.as_slice()) {
        ([begin], [end]) if begin < end => {
            let mut text = String::with_capacity(existing.len() + block.len());
            text.push_str(&existing[..*begin]);
            text.push_str(&block);
            text.push_str(&existing[*end..]);
            Ok(Applied {
                text,
                had_block: true,
            })
        }
        ([], []) => {
            let position = match place {
                Place::End => existing.len(),
                Place::AfterTitle => after_title(existing),
            };
            let (before, after) = existing.split_at(position);
            let mut text = String::with_capacity(existing.len() + block.len() + 4);
            text.push_str(before);
            if !before.is_empty() {
                if !before.ends_with('\n') {
                    text.push_str(eol);
                }
                if !before.ends_with("\n\n") && !before.ends_with("\n\r\n") {
                    text.push_str(eol);
                }
            }
            text.push_str(&block);
            text.push_str(eol);
            if !after.is_empty() && !after.starts_with('\n') && !after.starts_with("\r\n") {
                text.push_str(eol);
            }
            text.push_str(after);
            Ok(Applied {
                text,
                had_block: false,
            })
        }
        _ => Err(failure(format!(
            "{name} has an unbalanced or repeated ccid:security block: keep exactly one begin line and one end line, or remove both"
        ))),
    }
}
