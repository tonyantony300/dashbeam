//! Interactive approver for `dashbeam up` in a terminal.

use agent::approver::{async_trait, Approver};
use std::io::{BufRead, Write};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

/// Bidi controls that can visually reorder the rest of a line.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Makes a peer-supplied string (display name, file name…) safe to print on a
/// terminal: drops control characters (ESC, C1, newlines) and bidi overrides,
/// so a name can't rewrite or hide the rest of the line (e.g. the fingerprint).
pub fn display_safe(s: &str) -> String {
    s.chars()
        .filter(|&c| !c.is_control() && !is_bidi_control(c))
        .collect()
}

/// Like [`display_safe`] but keeps newlines and tabs, for multi-line messages
/// (errors listing candidate devices).
pub fn display_safe_multiline(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || c == '\t' || (!c.is_control() && !is_bidi_control(c)))
        .collect()
}

/// First 8 hex chars of an endpoint id, shown next to names in prompts.
pub fn short_fingerprint(endpoint_id: &str) -> String {
    display_safe(endpoint_id).chars().take(8).collect()
}

/// `y`/`yes` (any case) is yes; anything else, including empty, is no.
pub fn parse_answer(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Asks on stderr and reads `y/N` from stdin. One question at a time; no
/// answer within `timeout` (or stdin closed) counts as "no".
pub struct TtyApprover {
    timeout: Duration,
    lines: Mutex<mpsc::UnboundedReceiver<String>>,
}

impl TtyApprover {
    /// Reads stdin on a dedicated thread for the life of the process.
    pub fn new(timeout: Duration) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("dashbeam-stdin".into())
            .spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn stdin reader");
        Self::with_lines(rx, timeout)
    }

    /// Answers come from `lines` instead of stdin (for tests).
    pub fn with_lines(lines: mpsc::UnboundedReceiver<String>, timeout: Duration) -> Self {
        Self {
            timeout,
            lines: Mutex::new(lines),
        }
    }

    async fn ask(&self, question: &str) -> bool {
        let mut lines = self.lines.lock().await;
        // Drop anything typed while no question was showing.
        while lines.try_recv().is_ok() {}
        eprint!("\n{question} [y/N] ");
        let _ = std::io::stderr().flush();
        match tokio::time::timeout(self.timeout, lines.recv()).await {
            Ok(Some(answer)) => parse_answer(&answer),
            Ok(None) => false,
            Err(_) => {
                eprintln!("\nno answer, declined");
                false
            }
        }
    }
}

#[async_trait]
impl Approver for TtyApprover {
    async fn approve_pairing(&self, name: &str, endpoint_id: &str) -> bool {
        let q = format!(
            "Pair with \"{}\" ({})?",
            display_safe(name),
            short_fingerprint(endpoint_id)
        );
        self.ask(&q).await
    }

    async fn approve_invite(
        &self,
        name: &str,
        endpoint_id: &str,
        file_count: u32,
        total_size: u64,
    ) -> bool {
        let files = if file_count == 1 { "file" } else { "files" };
        let q = format!(
            "Accept {file_count} {files} ({}) from \"{}\" ({})?",
            format_bytes(total_size),
            display_safe(name),
            short_fingerprint(endpoint_id)
        );
        self.ask(&q).await
    }

    // A person is at the terminal, so paired-untrusted invites are asked here.
    fn answers_paired_invites(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_and_fingerprint() {
        assert!(parse_answer("y"));
        assert!(parse_answer(" YES \n"));
        assert!(!parse_answer(""));
        assert!(!parse_answer("n"));
        assert!(!parse_answer("yep"));
        assert_eq!(short_fingerprint("0123456789abcdef"), "01234567");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500_000), "1.5 MB");
    }

    #[test]
    fn display_safe_strips_terminal_and_bidi_controls() {
        // ESC sequences: clear line, move cursor, recolor; plus a C1 CSI.
        let evil = "Mallory\x1b[2K\r\x1b[1A\x1b[32mTrusted Mac\u{9b}31m\x07";
        let safe = display_safe(evil);
        assert!(!safe.chars().any(|c| c.is_control()), "{safe:?}");
        assert_eq!(safe, "Mallory[2K[1A[32mTrusted Mac31m");
        // RLO/PDI etc. would visually reorder the fingerprint after the name.
        let bidi = "Bob\u{202E}fdcba\u{202C}\u{2066}x\u{2069}\u{200F}";
        assert_eq!(display_safe(bidi), "Bobfdcbax");
        assert_eq!(display_safe("Tony's Mac Agent"), "Tony's Mac Agent");
        assert_eq!(
            display_safe_multiline("a\x1b[2J\n  b\u{202E}c"),
            "a[2J\n  bc"
        );
    }

    #[tokio::test]
    async fn yes_no_timeout_and_stale_input() {
        let (tx, rx) = mpsc::unbounded_channel();
        let a = TtyApprover::with_lines(rx, Duration::from_millis(200));
        assert!(a.answers_paired_invites());

        // Typed before the question: ignored, then times out to "no".
        tx.send("y".into()).unwrap();
        assert!(!a.approve_pairing("Box", "abcdef0123").await);

        let tx2 = tx.clone();
        let answer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            tx2.send("y".into()).unwrap();
        });
        assert!(a.approve_invite("Box", "abcdef0123", 2, 10).await);
        answer.await.unwrap();

        drop(tx);
        assert!(
            !a.approve_pairing("Box", "abcdef0123").await,
            "closed stdin is no"
        );
    }
}
