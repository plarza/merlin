//! Sandboxed code execution.
//!
//! Runs as a distinct `merlin-exec` uid rather than the bot's own, for two
//! reasons: that user cannot read `/var/lib/merlin` (the memory database, the
//! cron store) or the EnvironmentFile holding every API key, and a separate uid
//! is something nftables can match on, which is how LAN egress gets denied
//! while public internet stays reachable.
//!
//! The privilege step is `sudo -u merlin-exec <wrapper>`: sudo to an
//! unprivileged user, restricted to a single binary.

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct Sandbox {
    /// Command prefix, e.g. `["sudo", "-u", "merlin-exec", "/…/merlin-sandbox"]`.
    /// Configurable so tests and local runs can execute directly.
    runner: Vec<String>,
    timeout: Duration,
    max_output: usize,
    /// Passed to the wrapper, which owns the cgroup limit. Enforcing it here
    /// would be advisory only, since the child is a different user.
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
            .env("MERLIN_EXEC_MEMORY_MAX", &self.memory_max)
            .env("MERLIN_EXEC_TIMEOUT", self.timeout.as_secs().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning sandbox via {program}"))?;

        // Source arrives on stdin rather than as a temp file, so nothing the
        // agent writes ever lands on a filesystem the bot user can see.
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
            // kill_on_drop reaps the child; the wrapper also carries its own
            // hard limit so a wedged process dies even if we are not around.
            Err(_) => Ok(Output {
                stdout: String::new(),
                stderr: format!("execution exceeded {}s and was killed", self.timeout.as_secs()),
                exit_code: None,
                timed_out: true,
            }),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "é".repeat(100); // 2 bytes each
        let out = truncate(&s, 51);
        assert!(out.starts_with('é'));
        assert!(out.contains("truncated"));
    }

    #[tokio::test]
    async fn runs_and_captures_stdout() {
        // Direct runner: no sudo, no wrapper — exercises the plumbing only.
        let sb = Sandbox::new(vec!["/bin/sh".into(), "-c".into(), "cat >/dev/null; echo ok".into()], 10, "1G".into());
        let out = sb.run("python", "print(1)", None).await.unwrap();
        assert_eq!(out.stdout.trim(), "ok");
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn timeout_is_reported_not_hung() {
        let sb = Sandbox::new(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()], 1, "1G".into());
        let out = sb.run("bash", "true", None).await.unwrap();
        assert!(out.timed_out);
        assert!(out.stderr.contains("killed"));
    }
}
