//! Reading an agent's screen, as herdr hands it over (`herdr::read`).
//!
//! herdr keeps the screen; seagents only asks two things of it: what the
//! agent's input box holds — `send` must not paste onto someone's draft —
//! and whether a given dialog is up (`stop` must not type into one).

/// What the agent's input box holds, from its screen as herdr reads it with
/// styling (`--format ansi`): `Some("")` when it is empty or shows only its
/// placeholder, `Some(text)` when something is typed in it, None when no box
/// is on screen — nothing to go by.
///
/// The box is the last row that starts with `glyph` (Claude's `❯`, Codex's
/// `›`) and the rows under it, up to a blank row or a horizontal rule. A
/// placeholder (`Try "…"`, a suggested prompt, `Ask Codex to do anything`) is
/// drawn **dim** (SGR 2) and typed text is not, so only text that is not dim
/// counts as typed (measured on Claude 2.1.28x through herdr 0.9.0). Unlike
/// telling them apart by where the cursor sits, this also reads a draft whose
/// cursor was moved to its start as a draft.
pub(crate) fn input_box(ansi: &str, glyph: &str) -> Option<String> {
    let rows: Vec<Vec<(char, bool)>> = ansi.split('\n').map(styled_row).collect();
    let plain = |r: &[(char, bool)]| r.iter().map(|(c, _)| *c).collect::<String>();
    let top = rows
        .iter()
        .rposition(|r| plain(r).trim_start().starts_with(glyph))?;
    let rule = |r: &[(char, bool)]| plain(r).trim_start().starts_with('─');
    let end = rows[top + 1..]
        .iter()
        .position(|r| plain(r).trim().is_empty() || rule(r))
        .map_or(rows.len(), |n| top + 1 + n);
    // Typed text: the cells that are not dim, the glyph itself left out.
    let typed = |r: &[(char, bool)]| {
        r.iter()
            .filter(|(_, dim)| !dim)
            .map(|(c, _)| *c)
            .collect::<String>()
    };
    let first = typed(&rows[top]);
    let first = first
        .trim_start()
        .strip_prefix(glyph)
        .unwrap_or(&first)
        .trim()
        .to_string();
    let lines: Vec<String> = std::iter::once(first)
        .chain(
            rows[top + 1..end]
                .iter()
                .map(|r| typed(r).trim().to_string()),
        )
        .collect();
    let text = lines.join("\n");
    Some(if text.trim().is_empty() {
        String::new()
    } else {
        text.trim().to_string()
    })
}

/// Whether the plain screen shows any of `needles` — compared with
/// box-drawing characters dropped and whitespace runs as one space, so a
/// dialog drawn in a box, wrapped across rows or overdrawn by another still
/// matches its question.
pub(crate) fn shows_any(screen: &str, needles: &[String]) -> bool {
    let screen = normalized(screen);
    needles
        .iter()
        .map(|n| normalized(n))
        .any(|n| !n.is_empty() && screen.contains(&n))
}

/// `text` with box-drawing characters (U+2500–U+257F) and `|` gone and every
/// run of whitespace one space.
fn normalized(text: &str) -> String {
    let kept: String = text
        .chars()
        .map(|c| {
            if ('\u{2500}'..='\u{257f}').contains(&c) || c == '|' {
                ' '
            } else {
                c
            }
        })
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One row of an ANSI screen as (character, dim) cells: escape sequences
/// dropped, SGR 2 / 22 / 0 tracked. Each row starts undimmed — herdr resets
/// the style at the start of every styled run.
fn styled_row(line: &str) -> Vec<(char, bool)> {
    let mut out = Vec::new();
    let mut dim = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut last = None;
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            last = Some(c);
                            break;
                        }
                        params.push(c);
                    }
                    if last == Some('m') {
                        dim = sgr_dim(&params, dim);
                    }
                }
                // OSC and the like: up to BEL or ST.
                Some(']') | Some('P') | Some('_') | Some('^') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            c => out.push((c, dim)),
        }
    }
    out
}

/// The dim attribute after one SGR sequence's parameters (`2;38;2;1;2;3`).
/// Colour parameters carry numbers of their own (`38;2;r;g;b`, `38;5;n`),
/// which are skipped, not read as attributes.
fn sgr_dim(params: &str, mut dim: bool) -> bool {
    let p: Vec<u32> = params
        .split([';', ':'])
        .map(|x| x.parse().unwrap_or(0))
        .collect();
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            0 => dim = false,
            2 => dim = true,
            22 => dim = false,
            38 | 48 | 58 => {
                i += match p.get(i + 1) {
                    Some(5) => 2,
                    Some(2) => 4,
                    _ => 0,
                };
            }
            _ => {}
        }
        i += 1;
    }
    dim
}
