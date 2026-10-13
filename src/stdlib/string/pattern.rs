//! Lua patterns, ported from PUC-Rio Lua's `lstrlib.c` with indices instead of pointers.
//!
//! The matcher is recursive, like PUC-Rio Lua's: its recursion is capped at [`MAX_DEPTH`] levels
//! (as `MAXCCALLS` in Lua 5.4), and it counts its steps so that a single match attempt can be
//! capped (see [`Matcher::new`]) and charged fuel.

use thiserror::Error;

// DELIBERATE(puc): capped in Lua51 mode too (5.1 has no cap and overflows the C stack; see COMPATIBILITY.md).
/// The most nested calls of the matcher (as Lua 5.4's `MAXCCALLS`).
pub const MAX_DEPTH: usize = 200;

/// The most captures in a pattern (as `LUA_MAXCAPTURES`).
pub const MAX_CAPTURES: usize = 32;

const ESC: u8 = b'%';

/// A bracket class costs a step per this many bytes each time it is scanned (to find its end, or
/// to test a character), on top of the step of the pattern item.
const CLASS_BYTES_PER_STEP: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PatternError {
    #[error("pattern too complex")]
    TooComplex,
    #[error("malformed pattern (ends with '%')")]
    EndsWithEscape,
    #[error("malformed pattern (missing ']')")]
    MissingBracket,
    #[error("malformed pattern (missing arguments to '%b')")]
    MissingBalanceArguments,
    #[error("missing '[' after '%f' in pattern")]
    MissingFrontierBracket,
    #[error("invalid capture index %{0}")]
    InvalidCaptureIndex(i64),
    #[error("invalid pattern capture")]
    InvalidPatternCapture,
    #[error("too many captures")]
    TooManyCaptures,
    #[error("unfinished capture")]
    UnfinishedCapture,
}

/// Whether a pattern has no special characters, so it can be searched for as a plain string.
pub fn no_specials(pattern: &[u8]) -> bool {
    !pattern.iter().any(|c| {
        matches!(
            c,
            b'^' | b'$' | b'*' | b'+' | b'?' | b'.' | b'(' | b'[' | b'%' | b'-'
        )
    })
}

/// A capture of a successful match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// The bytes `start..start + len` of the subject.
    Bytes { start: usize, len: usize },
    /// A position capture `()`, 1-based.
    Position(usize),
}

#[derive(Debug, Clone, Copy)]
enum CaptureLen {
    Unfinished,
    Position,
    Len(usize),
}

pub struct Matcher<'a> {
    src: &'a [u8],
    pat: &'a [u8],
    lua51: bool,
    level: usize,
    capture: [(usize, CaptureLen); MAX_CAPTURES],
    depth: usize,
    steps: u64,
    max_steps: u64,
    // The step count at which the current attempt fails.
    limit: u64,
}

impl<'a> Matcher<'a> {
    /// A matcher of `pattern` (without its `^` anchor) in `subject`. `lua51` selects Lua 5.1's
    /// character classes (without `%g`). A match attempt taking more than `max_steps` steps is a
    /// [`PatternError::TooComplex`] error.
    pub fn new(subject: &'a [u8], pattern: &'a [u8], lua51: bool, max_steps: u64) -> Self {
        Matcher {
            src: subject,
            pat: pattern,
            lua51,
            level: 0,
            capture: [(0, CaptureLen::Unfinished); MAX_CAPTURES],
            depth: MAX_DEPTH,
            steps: 0,
            max_steps,
            limit: max_steps,
        }
    }

    /// The steps taken by all match attempts so far.
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Try to match the pattern at `start`, returning the end of the match.
    pub fn find_at(&mut self, start: usize) -> Result<Option<usize>, PatternError> {
        self.level = 0;
        self.depth = MAX_DEPTH;
        self.limit = self.steps.saturating_add(self.max_steps);
        self.do_match(start, 0)
    }

    /// The number of captures of the last successful match: with `whole`, at least one (the whole
    /// match); without it, zero when the pattern has no captures.
    pub fn capture_count(&self, whole: bool) -> usize {
        if self.level == 0 && whole {
            1
        } else {
            self.level
        }
    }

    /// The `i`-th capture of the last successful match, from `start` to `end`. With no captures,
    /// capture 0 is the whole match.
    pub fn capture(&self, i: usize, start: usize, end: usize) -> Result<Capture, PatternError> {
        if i >= self.level {
            if i != 0 {
                return Err(PatternError::InvalidCaptureIndex(i as i64 + 1));
            }
            return Ok(Capture::Bytes {
                start,
                len: end - start,
            });
        }
        match self.capture.get(i) {
            Some(&(init, CaptureLen::Len(len))) => Ok(Capture::Bytes { start: init, len }),
            Some(&(init, CaptureLen::Position)) => Ok(Capture::Position(init + 1)),
            _ => Err(PatternError::UnfinishedCapture),
        }
    }

    fn pat_at(&self, p: usize) -> u8 {
        // Past the end reads a zero byte, like the terminator of a C string.
        self.pat.get(p).copied().unwrap_or(0)
    }

    fn src_at(&self, s: usize) -> u8 {
        self.src.get(s).copied().unwrap_or(0)
    }

    fn step(&mut self) -> Result<(), PatternError> {
        self.steps += 1;
        if self.steps > self.limit {
            Err(PatternError::TooComplex)
        } else {
            Ok(())
        }
    }

    fn class_end(&mut self, p: usize) -> Result<usize, PatternError> {
        let c = self.pat_at(p);
        let start = p;
        let mut p = p + 1;
        match c {
            ESC => {
                if p >= self.pat.len() {
                    return Err(PatternError::EndsWithEscape);
                }
                Ok(p + 1)
            }
            b'[' => {
                if self.pat_at(p) == b'^' {
                    p += 1;
                }
                // Look for a ']'.
                loop {
                    if p >= self.pat.len() {
                        return Err(PatternError::MissingBracket);
                    }
                    let c = self.pat_at(p);
                    p += 1;
                    if c == ESC && p < self.pat.len() {
                        // Skip escapes (e.g. '%]').
                        p += 1;
                    }
                    if self.pat_at(p) == b']' && p < self.pat.len() {
                        break;
                    }
                }
                self.charge_class(p - start);
                Ok(p + 1)
            }
            _ => Ok(p),
        }
    }

    // Counts the steps of scanning a bracket class of `len` bytes.
    fn charge_class(&mut self, len: usize) {
        self.steps = self.steps.saturating_add(len as u64 / CLASS_BYTES_PER_STEP);
    }

    fn match_class(&self, c: u8, cl: u8) -> bool {
        let res = match cl.to_ascii_lowercase() {
            b'a' => c.is_ascii_alphabetic(),
            b'c' => c.is_ascii_control(),
            b'd' => c.is_ascii_digit(),
            b'g' if !self.lua51 => c.is_ascii_graphic(),
            b'l' => c.is_ascii_lowercase(),
            b'p' => c.is_ascii_punctuation(),
            b's' => matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'),
            b'u' => c.is_ascii_uppercase(),
            b'w' => c.is_ascii_alphanumeric(),
            b'x' => c.is_ascii_hexdigit(),
            b'z' => c == 0,
            _ => return cl == c,
        };
        if cl.is_ascii_lowercase() { res } else { !res }
    }

    // `p` is the '[' of the class, `ec` its ']'.
    fn match_bracket_class(&self, c: u8, p: usize, ec: usize) -> bool {
        let mut p = p;
        let mut sig = true;
        if self.pat_at(p + 1) == b'^' {
            sig = false;
            p += 1;
        }
        loop {
            p += 1;
            if p >= ec {
                break;
            }
            let pc = self.pat_at(p);
            if pc == ESC {
                p += 1;
                if self.match_class(c, self.pat_at(p)) {
                    return sig;
                }
            } else if self.pat_at(p + 1) == b'-' && p + 2 < ec {
                p += 2;
                if pc <= c && c <= self.pat_at(p) {
                    return sig;
                }
            } else if pc == c {
                return sig;
            }
        }
        !sig
    }

    fn single_match(&mut self, s: usize, p: usize, ep: usize) -> bool {
        if s >= self.src.len() {
            return false;
        }
        let c = self.src_at(s);
        match self.pat_at(p) {
            b'.' => true,
            ESC => self.match_class(c, self.pat_at(p + 1)),
            b'[' => {
                self.charge_class(ep - p);
                self.match_bracket_class(c, p, ep - 1)
            }
            pc => pc == c,
        }
    }

    fn match_balance(&mut self, s: usize, p: usize) -> Result<Option<usize>, PatternError> {
        if p + 1 >= self.pat.len() {
            return Err(PatternError::MissingBalanceArguments);
        }
        if s >= self.src.len() || self.src_at(s) != self.pat_at(p) {
            return Ok(None);
        }
        let b = self.pat_at(p);
        let e = self.pat_at(p + 1);
        let mut cont = 1;
        let mut s = s + 1;
        while s < self.src.len() {
            self.step()?;
            let c = self.src_at(s);
            if c == e {
                cont -= 1;
                if cont == 0 {
                    return Ok(Some(s + 1));
                }
            } else if c == b {
                cont += 1;
            }
            s += 1;
        }
        Ok(None)
    }

    fn max_expand(&mut self, s: usize, p: usize, ep: usize) -> Result<Option<usize>, PatternError> {
        let mut i = 0;
        while self.single_match(s + i, p, ep) {
            self.step()?;
            i += 1;
        }
        // Keep trying to match with the maximum repetitions.
        loop {
            if let Some(res) = self.do_match(s + i, ep + 1)? {
                return Ok(Some(res));
            }
            if i == 0 {
                return Ok(None);
            }
            i -= 1;
        }
    }

    fn min_expand(&mut self, s: usize, p: usize, ep: usize) -> Result<Option<usize>, PatternError> {
        let mut s = s;
        loop {
            if let Some(res) = self.do_match(s, ep + 1)? {
                return Ok(Some(res));
            } else if self.single_match(s, p, ep) {
                // Try with one more repetition.
                s += 1;
            } else {
                return Ok(None);
            }
        }
    }

    fn start_capture(
        &mut self,
        s: usize,
        p: usize,
        what: CaptureLen,
    ) -> Result<Option<usize>, PatternError> {
        let level = self.level;
        let Some(slot) = self.capture.get_mut(level) else {
            return Err(PatternError::TooManyCaptures);
        };
        *slot = (s, what);
        self.level = level + 1;
        let res = self.do_match(s, p)?;
        if res.is_none() {
            // Undo the capture.
            self.level -= 1;
        }
        Ok(res)
    }

    fn end_capture(&mut self, s: usize, p: usize) -> Result<Option<usize>, PatternError> {
        let l = self.capture_to_close()?;
        let init = self.capture.get(l).map_or(0, |c| c.0);
        if let Some(c) = self.capture.get_mut(l) {
            c.1 = CaptureLen::Len(s - init);
        }
        let res = self.do_match(s, p)?;
        if res.is_none() {
            // Undo the capture.
            if let Some(c) = self.capture.get_mut(l) {
                c.1 = CaptureLen::Unfinished;
            }
        }
        Ok(res)
    }

    fn capture_to_close(&self) -> Result<usize, PatternError> {
        (0..self.level)
            .rev()
            .find(|&l| matches!(self.capture.get(l), Some((_, CaptureLen::Unfinished))))
            .ok_or(PatternError::InvalidPatternCapture)
    }

    fn match_capture(&mut self, s: usize, l: u8) -> Result<Option<usize>, PatternError> {
        let index = i64::from(l) - i64::from(b'1');
        let capture = usize::try_from(index)
            .ok()
            .filter(|&l| l < self.level)
            .and_then(|l| self.capture.get(l));
        let (init, len) = match capture {
            Some(&(init, CaptureLen::Len(len))) => (init, len),
            // A position capture never matches.
            Some(&(_, CaptureLen::Position)) => return Ok(None),
            _ => return Err(PatternError::InvalidCaptureIndex(index + 1)),
        };
        self.steps = self.steps.saturating_add(len as u64 / 16);
        let captured = self.src.get(init..init + len);
        let here = self.src.get(s..s + len);
        Ok(if captured.is_some() && captured == here {
            Some(s + len)
        } else {
            None
        })
    }

    fn do_match(&mut self, s: usize, p: usize) -> Result<Option<usize>, PatternError> {
        if self.depth == 0 {
            return Err(PatternError::TooComplex);
        }
        self.depth -= 1;
        let result = self.do_match_inner(s, p);
        self.depth += 1;
        result
    }

    fn do_match_inner(
        &mut self,
        mut s: usize,
        mut p: usize,
    ) -> Result<Option<usize>, PatternError> {
        // A loop instead of tail calls.
        loop {
            self.step()?;
            if p >= self.pat.len() {
                // End of the pattern.
                return Ok(Some(s));
            }
            match self.pat_at(p) {
                b'(' => {
                    // Start capture.
                    return if self.pat_at(p + 1) == b')' {
                        self.start_capture(s, p + 2, CaptureLen::Position)
                    } else {
                        self.start_capture(s, p + 1, CaptureLen::Unfinished)
                    };
                }
                b')' => {
                    // End capture.
                    return self.end_capture(s, p + 1);
                }
                b'$' if p + 1 == self.pat.len() => {
                    // The end of the subject.
                    return Ok((s == self.src.len()).then_some(s));
                }
                ESC if self.pat_at(p + 1) == b'b' => {
                    // Balanced string.
                    match self.match_balance(s, p + 2)? {
                        Some(e) => {
                            s = e;
                            p += 4;
                            continue;
                        }
                        None => return Ok(None),
                    }
                }
                ESC if self.pat_at(p + 1) == b'f' => {
                    // Frontier.
                    p += 2;
                    if self.pat_at(p) != b'[' {
                        return Err(PatternError::MissingFrontierBracket);
                    }
                    let ep = self.class_end(p)?;
                    self.charge_class(2 * (ep - p));
                    let previous = if s == 0 { 0 } else { self.src_at(s - 1) };
                    if !self.match_bracket_class(previous, p, ep - 1)
                        && self.match_bracket_class(self.src_at(s), p, ep - 1)
                    {
                        p = ep;
                        continue;
                    }
                    return Ok(None);
                }
                ESC if self.pat_at(p + 1).is_ascii_digit() => {
                    // Back reference to a capture (%0-%9).
                    match self.match_capture(s, self.pat_at(p + 1))? {
                        Some(e) => {
                            s = e;
                            p += 2;
                            continue;
                        }
                        None => return Ok(None),
                    }
                }
                _ => {
                    // A pattern class plus an optional suffix.
                    let ep = self.class_end(p)?;
                    let suffix = self.pat_at(ep);
                    let suffix = if ep < self.pat.len() { suffix } else { 0 };
                    if !self.single_match(s, p, ep) {
                        if matches!(suffix, b'*' | b'?' | b'-') {
                            // Accept empty.
                            p = ep + 1;
                            continue;
                        }
                        // '+' or no suffix.
                        return Ok(None);
                    }
                    // Matched once.
                    match suffix {
                        b'?' => {
                            if let Some(res) = self.do_match(s + 1, ep + 1)? {
                                return Ok(Some(res));
                            }
                            p = ep + 1;
                            continue;
                        }
                        b'+' => return self.max_expand(s + 1, p, ep),
                        b'*' => return self.max_expand(s, p, ep),
                        b'-' => return self.min_expand(s, p, ep),
                        _ => {
                            s += 1;
                            p = ep;
                            continue;
                        }
                    }
                }
            }
        }
    }
}

/// How a plain search ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plain {
    /// The needle is at this position.
    Found(usize),
    Missing,
    /// The search compared its share of bytes and goes on from this position.
    Paused(usize),
}

/// Searches `haystack` for `needle` from `start` (as `lmemfind` does), pausing once about `limit`
/// bytes are compared, and returns the outcome and the number of bytes compared.
///
/// A search can compare `haystack.len() * needle.len()` bytes (a needle of `a`s ending in `b` in
/// a haystack of `a`s), so it pauses rather than running to the end in one go. It compares at
/// least one candidate position, or 1 byte, before pausing.
pub fn find_plain(
    haystack: &[u8],
    needle: &[u8],
    mut start: usize,
    limit: usize,
) -> (Plain, usize) {
    let Some((&first, rest)) = needle.split_first() else {
        return (Plain::Found(start), 0);
    };
    let Some(last_start) = haystack.len().checked_sub(needle.len()) else {
        return (Plain::Missing, 0);
    };
    let mut compared = 0;
    while start <= last_start {
        if compared > 0 && compared >= limit {
            return (Plain::Paused(start), compared);
        }
        let scan = limit.saturating_sub(compared).max(1);
        let end = last_start.min(start.saturating_add(scan - 1));
        let window = haystack.get(start..=end).unwrap_or_default();
        let Some(offset) = window.iter().position(|&c| c == first) else {
            compared += window.len();
            start = end + 1;
            continue;
        };
        let i = start + offset;
        compared += offset + 1 + rest.len();
        if haystack.get(i + 1..i + needle.len()) == Some(rest) {
            return (Plain::Found(i), compared);
        }
        start = i + 1;
    }
    (Plain::Missing, compared)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Searches to the end, a few bytes at a time.
    fn find_plain_all(haystack: &[u8], needle: &[u8], limit: usize) -> (Option<usize>, usize) {
        let (mut start, mut total) = (0, 0);
        loop {
            let (result, compared) = find_plain(haystack, needle, start, limit);
            total += compared;
            match result {
                Plain::Found(i) => return (Some(i), total),
                Plain::Missing => return (None, total),
                Plain::Paused(next) => {
                    assert!(next > start && compared > 0);
                    start = next;
                }
            }
        }
    }

    #[test]
    fn plain() {
        for limit in [0, 1, 3, 1000] {
            assert_eq!(find_plain_all(b"hello world", b"o w", limit).0, Some(4));
            assert_eq!(find_plain_all(b"hello world", b"ow", limit).0, None);
            assert_eq!(find_plain_all(b"hello", b"", limit).0, Some(0));
            assert_eq!(find_plain_all(b"he", b"hello", limit).0, None);
            assert_eq!(find_plain_all(b"aab", b"ab", limit).0, Some(1));
        }
        assert_eq!(find_plain(b"hello", b"", 3, 0), (Plain::Found(3), 0));
        // A scan without candidates compares each byte once.
        let haystack = vec![b'a'; 10_000];
        assert_eq!(find_plain_all(&haystack, b"b", 100), (None, 10_000));
        assert_eq!(
            find_plain(&haystack, b"b", 0, 100),
            (Plain::Paused(100), 100)
        );
        // The worst case compares every candidate in full.
        let mut needle = vec![b'a'; 99];
        needle.push(b'b');
        let (result, compared) = find_plain_all(&haystack, &needle, 1_000);
        assert_eq!(result, None);
        assert_eq!(compared, (10_000 - 100 + 1) * 100);
        assert_eq!(
            find_plain(&haystack, &needle, 0, 1_000),
            (Plain::Paused(10), 1_000)
        );
    }

    fn find(s: &str, p: &str) -> Option<(usize, usize)> {
        let (anchor, p) = match p.strip_prefix('^') {
            Some(p) => (true, p),
            None => (false, p),
        };
        let mut m = Matcher::new(s.as_bytes(), p.as_bytes(), false, 1_000_000);
        for start in 0..=s.len() {
            if let Some(end) = m.find_at(start).unwrap() {
                return Some((start, end));
            }
            if anchor {
                break;
            }
        }
        None
    }

    #[test]
    fn test_patterns() {
        assert_eq!(find("hello world", "o w"), Some((4, 7)));
        assert_eq!(find("hello", "l+"), Some((2, 4)));
        assert_eq!(find("hello", "^h.-o$"), Some((0, 5)));
        assert_eq!(find("hello", "x*"), Some((0, 0)));
        assert_eq!(find("a(b(c)d)e", "%b()"), Some((1, 8)));
        assert_eq!(find("THE (quick) fox", "%f[%a]%a+"), Some((0, 3)));
        assert_eq!(find("abcabc", "(abc)%1"), Some((0, 6)));
        assert_eq!(find("key = value", "(%w+)%s*=%s*(%w+)"), Some((0, 11)));
        assert_eq!(find("[]", "[]]"), Some((1, 2)));
        assert_eq!(find("a-b", "[%-]"), Some((1, 2)));
    }

    #[test]
    fn test_pattern_limits() {
        let s = "a".repeat(30);
        let p = "a*".repeat(30) + "b";
        let mut m = Matcher::new(s.as_bytes(), p.as_bytes(), false, 1_000_000);
        assert_eq!(m.find_at(0), Err(PatternError::TooComplex));

        let p = "(".repeat(300) + &")".repeat(300);
        let mut m = Matcher::new(b"", p.as_bytes(), false, 1_000_000);
        assert!(m.find_at(0).is_err());
    }
}
