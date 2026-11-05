//! Platform-independent managed-section composition shared by desktop and NAS.

const CONTENT_START_MARKER: &str = "# --- SWITCHHOSTS_CONTENT_START ---";
const CONTENT_END_MARKER: &str = "# --- SWITCHHOSTS_CONTENT_END ---";

// ---- append-mode helper ----------------------------------------------------

/// Rebuild the hosts file around the SwitchHosts-managed section.
///
/// Layout: `head` (everything before START, untouched) + the managed
/// section wrapped in START/END markers + `tail` (everything after END,
/// untouched). Preserving the tail is what keeps entries other tools
/// append to the end of the file alive across re-applies.
///
/// When the aggregated content is empty but a tail exists, an empty
/// START/END anchor is kept in place: dropping it would make the next
/// non-empty apply re-create the section *after* the tail, silently
/// reordering entries (hosts resolution is first-match-wins).
///
/// The seams are normalised (head trimmed at its end, tail at both
/// ends, fixed blank lines around the markers) so that re-applying the
/// same content is byte-identical — the caller's byte comparison
/// depends on that to avoid needless privileged writes.
pub(crate) fn make_append_content(previous_lf: &str, new_content_lf: &str) -> String {
    let (head, tail) = split_around_managed_section(previous_lf);
    let head = head.trim_end();
    let tail = tail.trim();
    let content = strip_marker_lines(new_content_lf);
    let content = content.trim();

    let mut parts: Vec<String> = Vec::new();
    if !head.is_empty() {
        parts.push(head.to_string());
    }
    if !content.is_empty() {
        parts.push(format!(
            "{CONTENT_START_MARKER}\n\n{content}\n\n{CONTENT_END_MARKER}"
        ));
    } else if !tail.is_empty() {
        parts.push(format!("{CONTENT_START_MARKER}\n\n{CONTENT_END_MARKER}"));
    }
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }

    let mut result = parts.join("\n\n");
    result.push('\n');
    result
}

/// True when `line` is a section boundary. Markers only count as
/// boundaries on their own line (modulo surrounding whitespace) — a
/// marker quoted inside a hosts entry's inline comment is content, not
/// a boundary. Every SwitchHosts version has written markers on their
/// own line, so this stays backward-compatible.
fn is_marker_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == CONTENT_START_MARKER || trimmed == CONTENT_END_MARKER
}

/// Split the previous hosts content into (head, tail) around the
/// SwitchHosts-managed section.
///
/// The END marker is only searched for *after* the START marker, so a
/// stray END sitting before START stays part of the head and is never
/// touched. When no END follows START (files written by versions that
/// predate the END marker), the tail is empty: everything from START to
/// EOF is considered SwitchHosts-owned, matching the legacy behaviour.
fn split_around_managed_section(previous_lf: &str) -> (&str, &str) {
    let Some((start_begin, start_end)) = find_marker_line(previous_lf, 0, CONTENT_START_MARKER)
    else {
        return (previous_lf, "");
    };

    let head = &previous_lf[..start_begin];

    match find_marker_line(previous_lf, start_end, CONTENT_END_MARKER) {
        Some((_, end_end)) => (head, &previous_lf[end_end..]),
        None => (head, ""),
    }
}

/// Find the first line at or after byte offset `from` whose trimmed
/// content equals `marker`. Returns the line's `(start, end)` byte
/// offsets, `end` excluding the trailing newline.
fn find_marker_line(text: &str, from: usize, marker: &str) -> Option<(usize, usize)> {
    let mut line_start = from;
    for segment in text[from..].split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        if line.trim() == marker {
            return Some((line_start, line_start + line.len()));
        }
        line_start += segment.len();
    }
    None
}

/// Drop aggregated-content lines that *are* a marker line: left in,
/// such a line would make the next parse end the managed section early
/// and leak the remainder into the preserved tail forever. Lines merely
/// containing a marker substring are kept — they are not boundaries.
fn strip_marker_lines(content_lf: &str) -> String {
    if !content_lf.lines().any(is_marker_line) {
        return content_lf.to_string();
    }

    content_lf
        .lines()
        .filter(|line| !is_marker_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod shared_tests {
    #[test]
    fn matches_all_desktop_append_fixtures() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../test/fixtures/hosts_append_cases.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let mut content = case["previous"].as_str().unwrap().to_string();
            for apply in case["applies"].as_array().unwrap() {
                content = super::make_append_content(&content, apply.as_str().unwrap());
            }
            assert_eq!(
                content,
                case["expected"].as_str().unwrap(),
                "{}",
                case["name"]
            );
        }
    }
}
