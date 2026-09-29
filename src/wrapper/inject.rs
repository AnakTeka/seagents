//! Writing into the agent's input as if typed or pasted: the one
//! implementation of injection framing, for `send`/`stop` over the control
//! socket and for anything else the wrapper ever has to type for the agent.
//!
//! Settled by reading herdr (ORCHESTRATION-SPEC "Injection framing"):
//!
//! 1. Text goes in as a **bracketed paste** (`ESC[200~ … ESC[201~`) when the
//!    agent has asked for bracketed paste (DECSET 2004), raw otherwise — never
//!    typed key by key, and no single-/multi-line split: a paste is how a TUI
//!    takes a newline without submitting.
//! 2. The **submit is a second write**, `submit_delay_ms` later (300), so the
//!    TUI has consumed the paste before Enter arrives. The writer is released
//!    in between, so the human's own keystrokes are never held back.
//! 3. **Enter is encoded for the agent's keyboard protocol**: `\r`, unless it
//!    has pushed kitty keyboard flags that report every key as an escape code.
//! 4. An adapter may ask for a **pre-send nudge** (Copilot: a synthetic
//!    focus-in) ahead of the body.
//!
//! The modes come from the agent's own output: [`ModeScanner`] watches the
//! output pump's stream for the few sequences that set them. It only looks —
//! the bytes reach the terminal exactly as before.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The PTY master's input side, shared by the input pump (the human's
/// keystrokes, locked per chunk) and the control thread (locked for a whole
/// body, so a keystroke burst cannot land inside an injected prompt).
pub(super) type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Input modes the agent has switched on, as last seen in its output.
#[derive(Debug, Default)]
pub(super) struct PtyModes {
    bracketed_paste: AtomicBool,
    /// The kitty keyboard flags in effect on the active screen.
    kitty_flags: AtomicU32,
}

/// Kitty keyboard flag: every key, Enter included, is reported as an escape
/// code (`CSI 13 u`). Without it Enter stays `\r` — even under "disambiguate"
/// (flag 1) and under xterm's modifyOtherKeys, both of which only change
/// *modified* keys, which is why modifyOtherKeys is not tracked at all.
const KITTY_REPORT_ALL_KEYS: u32 = 0b1000;

/// Kitty's own bound on the flag stack; deeper pushes evict the oldest.
const KITTY_STACK_MAX: usize = 32;

impl PtyModes {
    fn bracketed_paste(&self) -> bool {
        self.bracketed_paste.load(Ordering::Relaxed)
    }

    fn kitty_flags(&self) -> u32 {
        self.kitty_flags.load(Ordering::Relaxed)
    }
}

/// Tracks, across output chunks, the control sequences that set the modes
/// framing depends on: DECSET/DECRST 2004 (bracketed paste), the alternate
/// screen (kitty keeps a separate flag stack per screen), and the kitty
/// keyboard protocol's push (`CSI > f u`), pop (`CSI < n u`) and set
/// (`CSI = f ; m u`). Everything else is skipped a byte at a time; a
/// sequence split across two reads is carried over.
#[derive(Debug, Default)]
pub(super) struct ModeScanner {
    state: Scan,
    /// The CSI's private marker (`?`, `>`, `<`, `=`) and parameter bytes.
    params: Vec<u8>,
    alt_screen: bool,
    /// Kitty flag stacks: [main, alternate].
    kitty: [Vec<u32>; 2],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    Esc,
    Csi,
}

/// Longest CSI parameter string worth keeping; anything longer is not one of
/// ours and is skipped without being buffered.
const PARAMS_MAX: usize = 32;

impl ModeScanner {
    pub(super) fn feed(&mut self, data: &[u8], modes: &PtyModes) {
        for &b in data {
            match self.state {
                Scan::Ground => {
                    if b == 0x1b {
                        self.state = Scan::Esc;
                    }
                }
                Scan::Esc => {
                    if b == b'[' {
                        self.state = Scan::Csi;
                        self.params.clear();
                    } else {
                        self.state = if b == 0x1b { Scan::Esc } else { Scan::Ground };
                    }
                }
                Scan::Csi => match b {
                    // Parameters; intermediates are kept as a byte none of
                    // ours has, so a sequence carrying one matches nothing.
                    0x20..=0x3f => {
                        if self.params.len() < PARAMS_MAX {
                            self.params.push(if b < 0x30 { b'!' } else { b });
                        }
                    }
                    0x40..=0x7e => {
                        self.finish(b, modes);
                        self.state = Scan::Ground;
                    }
                    0x1b => self.state = Scan::Esc,
                    // C0 controls inside a CSI are executed, not part of it.
                    _ => {}
                },
            }
        }
    }

    fn finish(&mut self, final_byte: u8, modes: &PtyModes) {
        let (marker, rest) = match self.params.split_first() {
            Some((m @ (b'?' | b'>' | b'<' | b'='), rest)) => (Some(*m), rest),
            _ => (None, &self.params[..]),
        };
        let Ok(text) = std::str::from_utf8(rest) else {
            return;
        };
        let nums: Vec<Option<u32>> = text.split(';').map(|p| p.parse().ok()).collect();
        match (marker, final_byte) {
            (Some(b'?'), b'h' | b'l') => {
                let on = final_byte == b'h';
                for n in nums.iter().flatten() {
                    match n {
                        2004 => modes.bracketed_paste.store(on, Ordering::Relaxed),
                        47 | 1047 | 1049 => self.alt_screen = on,
                        _ => {}
                    }
                }
            }
            (Some(b'>'), b'u') => {
                let stack = &mut self.kitty[usize::from(self.alt_screen)];
                if stack.len() == KITTY_STACK_MAX {
                    stack.remove(0);
                }
                stack.push(nums[0].unwrap_or(0));
            }
            (Some(b'<'), b'u') => {
                let stack = &mut self.kitty[usize::from(self.alt_screen)];
                let n = nums[0].unwrap_or(1).max(1) as usize;
                stack.truncate(stack.len().saturating_sub(n));
            }
            (Some(b'='), b'u') => {
                let stack = &mut self.kitty[usize::from(self.alt_screen)];
                let flags = nums[0].unwrap_or(0);
                let top = stack.last().copied().unwrap_or(0);
                let new = match nums.get(1).copied().flatten().unwrap_or(1) {
                    2 => top | flags,
                    3 => top & !flags,
                    _ => flags,
                };
                match stack.last_mut() {
                    Some(t) => *t = new,
                    None => stack.push(new),
                }
            }
            _ => return,
        }
        let active = self.kitty[usize::from(self.alt_screen)].last().copied().unwrap_or(0);
        modes.kitty_flags.store(active, Ordering::Relaxed);
    }
}

/// What to write.
#[derive(Debug, Clone, Copy)]
pub(super) enum Body<'a> {
    /// Text, pasted (bracketed when the agent has asked for it).
    Paste(&'a str),
    /// Bytes written verbatim — keys for a menu, say.
    Keys(&'a [u8]),
}

/// The adapter's `send` facts, as injection needs them.
#[derive(Debug, Clone, Copy)]
pub(super) struct Framing<'a> {
    pub(super) pre_send: &'a str,
    pub(super) submit_delay: Duration,
}

/// Write `body` into the agent, then — when `submit` — the Enter key after
/// the adapter's delay. The writer is held for the body alone.
pub(super) fn deliver(
    writer: &SharedWriter,
    modes: &PtyModes,
    body: Body,
    submit: bool,
    framing: Framing,
) -> Result<(), String> {
    let mut bytes = pre_send_bytes(framing.pre_send)?.to_vec();
    match body {
        Body::Paste(text) => bytes.extend(frame_paste(text, modes.bracketed_paste())),
        Body::Keys(keys) => bytes.extend_from_slice(keys),
    }
    write_all(writer, &bytes)?;
    if submit {
        std::thread::sleep(framing.submit_delay);
        write_all(writer, enter_key(modes.kitty_flags()))?;
    }
    Ok(())
}

fn write_all(writer: &SharedWriter, bytes: &[u8]) -> Result<(), String> {
    let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
    w.write_all(bytes)
        .and_then(|_| w.flush())
        .map_err(|e| format!("writing to the agent's terminal: {e}"))
}

/// The adapter's `pre_send` nudge.
fn pre_send_bytes(pre_send: &str) -> Result<&'static [u8], String> {
    match pre_send {
        "" => Ok(b""),
        // What a terminal reports when its window gains focus (DECSET 1004).
        "focus_gained" => Ok(b"\x1b[I"),
        other => Err(format!("unknown send.pre_send {other:?} in the agent's adapter")),
    }
}

/// `text` as a paste. Bracketed, the end marker is removed from the body
/// first: text that closed the paste itself would have the rest of it typed.
fn frame_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut out = b"\x1b[200~".to_vec();
    out.extend_from_slice(text.replace("\x1b[201~", "").as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// The Enter key, as the agent's keyboard protocol has it.
fn enter_key(kitty_flags: u32) -> &'static [u8] {
    if kitty_flags & KITTY_REPORT_ALL_KEYS != 0 {
        b"\x1b[13u"
    } else {
        b"\r"
    }
}

/// Decode the escapes `send --keys` takes: `\e` (ESC), `\r`, `\n`, `\t`,
/// `\0`, `\\`, and `\xNN` for any byte; everything else is taken literally
/// (as UTF-8). So `\e[B\r` is Down, then Enter.
pub(crate) fn decode_keys(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('e') => out.push(0x1b),
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                let byte = (hex.len() == 2)
                    .then(|| u8::from_str_radix(&hex, 16).ok())
                    .flatten()
                    .ok_or_else(|| format!("\\x{hex}: want two hex digits"))?;
                out.push(byte);
            }
            Some(other) => return Err(format!("unknown escape \\{other} (\\e \\r \\n \\t \\0 \\\\ \\xNN)")),
            None => return Err("a trailing \\ escapes nothing".to_string()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> (bool, u32) {
        let modes = PtyModes::default();
        let mut s = ModeScanner::default();
        for c in chunks {
            s.feed(c, &modes);
        }
        (modes.bracketed_paste(), modes.kitty_flags())
    }

    #[test]
    fn bracketed_paste_follows_decset_2004_even_split_across_reads() {
        assert_eq!(scan(&[b"hello"]), (false, 0));
        assert_eq!(scan(&[b"\x1b[?2004h"]), (true, 0));
        assert_eq!(scan(&[b"x\x1b[?20", b"04hy"]), (true, 0));
        assert_eq!(scan(&[b"\x1b", b"[?2004h"]), (true, 0));
        assert_eq!(scan(&[b"\x1b[?2004h", b"\x1b[?2004l"]), (false, 0));
        // Several modes in one sequence.
        assert_eq!(scan(&[b"\x1b[?1004;2004;25h"]), (true, 0));
        // Not ours: no private marker, other modes, SGR, OSC text.
        assert_eq!(scan(&[b"\x1b[2004h\x1b[?2003h\x1b[1;31m\x1b]0;?2004h\x07"]), (false, 0));
    }

    #[test]
    fn kitty_flags_follow_push_pop_and_set_per_screen() {
        assert_eq!(scan(&[b"\x1b[>1u"]).1, 1);
        assert_eq!(scan(&[b"\x1b[>1u\x1b[>31u"]).1, 31);
        assert_eq!(scan(&[b"\x1b[>1u\x1b[>31u\x1b[<u"]).1, 1);
        assert_eq!(scan(&[b"\x1b[>1u\x1b[>31u\x1b[<2u"]).1, 0);
        assert_eq!(scan(&[b"\x1b[<5u"]).1, 0, "popping an empty stack");
        assert_eq!(scan(&[b"\x1b[>1u\x1b[=8;2u"]).1, 9, "set, or-mode");
        assert_eq!(scan(&[b"\x1b[>9u\x1b[=1;3u"]).1, 8, "set, and-not mode");
        assert_eq!(scan(&[b"\x1b[>9u\x1b[=4u"]).1, 4, "set, replace");
        // The alternate screen has its own stack.
        assert_eq!(scan(&[b"\x1b[>8u\x1b[?1049h"]).1, 0, "the alternate screen's stack is empty");
        assert_eq!(scan(&[b"\x1b[>8u\x1b[?1049h\x1b[>1u"]).1, 1);
        assert_eq!(scan(&[b"\x1b[>8u\x1b[?1049h\x1b[>1u\x1b[?1049l"]).1, 8, "back to the main one");
        assert_eq!(scan(&[b"\x1b[>8u\x1b[?1049h\x1b[>1u\x1b[?1049l\x1b[<0u"]).1, 0);
    }

    #[test]
    fn a_paste_is_bracketed_only_when_asked_for_and_cannot_close_itself() {
        assert_eq!(frame_paste("a\nb", false), b"a\nb");
        assert_eq!(frame_paste("a\nb \"q\"", true), b"\x1b[200~a\nb \"q\"\x1b[201~");
        assert_eq!(frame_paste("x\x1b[201~rm -rf", true), b"\x1b[200~xrm -rf\x1b[201~");
    }

    #[test]
    fn enter_is_cr_unless_every_key_is_an_escape_code() {
        assert_eq!(enter_key(0), b"\r");
        assert_eq!(enter_key(1), b"\r", "disambiguate leaves Enter alone");
        assert_eq!(enter_key(0b10111), b"\r");
        assert_eq!(enter_key(8), b"\x1b[13u");
        assert_eq!(enter_key(31), b"\x1b[13u");
    }

    #[test]
    fn delivery_writes_nudge_body_then_the_delayed_enter() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        struct Tee(Arc<Mutex<Vec<Vec<u8>>>>);
        impl Write for Tee {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().push(b.to_vec());
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(Tee(Arc::clone(&sink)))));
        let modes = PtyModes::default();
        modes.bracketed_paste.store(true, Ordering::Relaxed);
        let framing = Framing { pre_send: "focus_gained", submit_delay: Duration::from_millis(1) };
        deliver(&writer, &modes, Body::Paste("hi"), true, framing).unwrap();
        assert_eq!(
            *sink.lock().unwrap(),
            [b"\x1b[I\x1b[200~hi\x1b[201~".to_vec(), b"\r".to_vec()],
            "one write for the body, a second for Enter"
        );
        sink.lock().unwrap().clear();
        let plain = Framing { pre_send: "", submit_delay: Duration::ZERO };
        deliver(&writer, &modes, Body::Keys(b"1"), false, plain).unwrap();
        assert_eq!(*sink.lock().unwrap(), [b"1".to_vec()]);
        let bad = Framing { pre_send: "wiggle", submit_delay: Duration::ZERO };
        assert!(deliver(&writer, &modes, Body::Keys(b"1"), false, bad).unwrap_err().contains("wiggle"));
    }

    #[test]
    fn key_escapes() {
        assert_eq!(decode_keys("1").unwrap(), b"1");
        assert_eq!(decode_keys(r"\e[B\r").unwrap(), b"\x1b[B\r");
        assert_eq!(decode_keys(r"a\tb\nc\\d\0").unwrap(), b"a\tb\nc\\d\0");
        assert_eq!(decode_keys(r"\x1b\x7F\xff").unwrap(), [0x1b, 0x7f, 0xff]);
        assert_eq!(decode_keys("é").unwrap(), "é".as_bytes());
        for bad in [r"\q", r"\x1", r"\xzz", "\\"] {
            assert!(decode_keys(bad).is_err(), "{bad}");
        }
    }
}
