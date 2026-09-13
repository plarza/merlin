use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct Sandbox {
    runner: Vec<String>,
    timeout: Duration,
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
            memory_max,
        }
    }

    pub async fn run(&self, source: &str, room_id: &str) -> Result<Output> {
        let (program, args) = self
            .runner
            .split_first()
            .context("exec runner is empty; nothing to invoke")?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .arg(self.timeout.as_secs().to_string())
            .arg(address_space_kb(&self.memory_max).to_string())
            .arg(crate::db::room_key(room_id))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning sandbox via {program}"))?;

        if let Some(mut sink) = child.stdin.take() {
            sink.write_all(source.as_bytes()).await.ok();
            sink.shutdown().await.ok();
        }

        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(result) => {
                let out = result.context("collecting sandbox output")?;
                Ok(Output {
                    stdout: crate::truncate(&String::from_utf8_lossy(&out.stdout), MAX_OUTPUT),
                    stderr: crate::truncate(&String::from_utf8_lossy(&out.stderr), MAX_OUTPUT),
                    exit_code: out.status.code(),
                    timed_out: false,
                })
            }
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
