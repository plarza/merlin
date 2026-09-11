//! Sandboxed code execution.
//!
//! Runs as a distinct `merlin-exec` uid rather than the bot's own, for two reasons: that user cannot read `/var/lib/merlin` (the memory database,
//! the cron store) or the EnvironmentFile holding every API key, and a separate uid is something the firewall can match on, which is how LAN egress gets denied while public internet stays reachable.
//!
//! The privilege step is `sudo -u merlin-exec <wrapper>`: sudo to an unprivileged user, restricted to a single binary.

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct Sandbox {
    /// Command prefix, e.g.
    /// `["sudo", "-u", "merlin-exec", "/…/merlin-sandbox"]`.
    /// Configurable so tests and local runs can execute directly.
    runner: Vec<String>,
    timeout: Duration,
    max_output: usize,
    /// Passed to the wrapper as an address-space limit.
    /// Enforcing it here would be advisory only, since the child is a different user.
    memory_max: String,
}

pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

const MAX_OUTPUT: usize = 24_000;

impl Sandbox {
    pub fn new(runner: Vec<String>, timeout_s: u64, memory_max: String) -> Self {
        Self {
            runner,
            timeout: Duration::from_secs(timeout_s),
            max_output: MAX_OUTPUT,
            memory_max,
        }
    }

    pub async fn run(&self, language: &str, source: &str, stdin: Option<&str>) -> Result<Output> {
        let lang = normalize_language(language)?;

        let (program, args) = self
            .runner
            .split_first()
            .context("exec runner is empty; nothing to invoke")?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .arg(lang)
            .arg(self.timeout.as_secs().to_string())
            .arg(address_space_kb(&self.memory_max).to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning sandbox via {program}"))?;

        // Source arrives on stdin rather than as a temp file, so nothing the agent writes ever lands on a filesystem the bot user can see.
        if let Some(mut sink) = child.stdin.take() {
            let payload = match stdin {
                Some(extra) => format!("{source}\n\u{0}{extra}"),
                None => source.to_string(),
            };
            sink.write_all(payload.as_bytes()).await.ok();
            sink.shutdown().await.ok();
        }

        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(result) => {
                let out = result.context("collecting sandbox output")?;
                Ok(Output {
                    stdout: truncate(&String::from_utf8_lossy(&out.stdout), self.max_output),
                    stderr: truncate(&String::from_utf8_lossy(&out.stderr), self.max_output),
                    exit_code: out.status.code(),
                    timed_out: false,
                })
            }
            // kill_on_drop reaps the child; the wrapper also carries its own hard limit so a wedged process dies even if we are not around.
            Err(_) => Ok(Output {
                stdout: String::new(),
                stderr: format!(
                    "execution exceeded {}s and was killed",
                    self.timeout.as_secs()
                ),
                exit_code: None,
                timed_out: true,
            }),
        }
    }
}

/// Parse a size like `1G` or `512M` into kilobytes for `ulimit -v`.
/// An unparseable value yields 0, which the wrapper reads as no limit rather than as a limit of nothing.
fn address_space_kb(size: &str) -> u64 {
    let raw = size.trim();
    let (digits, scale) = match raw.chars().last() {
        Some('G') | Some('g') => (&raw[..raw.len() - 1], 1024 * 1024),
        Some('M') | Some('m') => (&raw[..raw.len() - 1], 1024),
        Some('K') | Some('k') => (&raw[..raw.len() - 1], 1),
        _ => (raw, 1),
    };
    digits.trim().parse::<u64>().unwrap_or(0) * scale
}

fn normalize_language(language: &str) -> Result<&'static str> {
    match language.trim().to_lowercase().as_str() {
        "python" | "python3" | "py" => Ok("python"),
        "bash" | "sh" | "shell" => Ok("bash"),
        other => anyhow::bail!("unsupported language '{other}'; use python or bash"),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Cut on a char boundary so the result stays valid UTF-8.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… truncated at {} bytes", &s[..end], max)
}
