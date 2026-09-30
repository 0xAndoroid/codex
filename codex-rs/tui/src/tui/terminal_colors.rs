//! Follow terminal light/dark switches without blocking the input loop.
//!
//! Codex enables DEC private mode 2031 alongside its other terminal modes, so the terminal
//! reports color-scheme changes as `CSI ? 997 ; Ps n`. Each report, and each focus gain, writes
//! OSC 10/11 default-color queries without waiting. Crossterm parses the replies from the normal
//! input stream into `Event::ColorReport`s, so no probe competes with typed input for terminal
//! bytes.

use std::fmt;

/// Default foreground (OSC 10) and background (OSC 11) color queries.
pub(super) const DEFAULT_COLOR_QUERY: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\";

/// Enables DEC private mode 2031 color-scheme change reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EnableColorSchemeReports;

impl crossterm::Command for EnableColorSchemeReports {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?2031h")
    }
}

/// Disables DEC private mode 2031 color-scheme change reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DisableColorSchemeReports;

impl crossterm::Command for DisableColorSchemeReports {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?2031l")
    }
}
