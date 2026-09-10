//! A persistent directory the agent can read, write and run code in.
//!
//! This is what separates a chat assistant from a coding one: files survive between turns, so the model can write a script, run it, read the error and fix it.
//! The same directory is bound into the sandbox at `/work`, so the shell sees exactly what these tools write.
//!
//! Only writing lives here. Reading, listing and searching are `cat`, `ls` and `rg` in the shell, which do the job already;
//! these two exist because the shell cannot do them safely: a heredoc has to guess a delimiter that the content does not contain, and `sed` will silently replace the wrong occurrence.
//!
//! Every path is resolved inside the root and rejected otherwise, including through symlinks, so the tools cannot reach the rest of the host.

use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

pub struct Workspace {
    root: PathBuf,
}

/// One exact-text replacement.
pub struct Edit {
    pub old: String,
    pub new: String,
}

impl Workspace {
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating workspace at {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a caller-supplied path inside the root.
    ///
    /// Traversal components are rejected rather than normalised away, and anything that already exists is checked after following symlinks,
    /// since code running in the sandbox shares this directory and could otherwise leave a link pointing out of it.
    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let mut out = self.root.clone();
        for component in Path::new(path.trim_start_matches('/')).components() {
            match component {
                Component::Normal(part) => out.push(part),
                Component::CurDir => {}
                _ => anyhow::bail!("'{path}' must stay inside the workspace"),
            }
        }

        if let Ok(real) = out.canonicalize()
            && !real.starts_with(
                self.root
                    .canonicalize()
                    .unwrap_or_else(|_| self.root.clone()),
            )
        {
            anyhow::bail!("'{path}' resolves outside the workspace");
        }
        Ok(out)
    }

    pub fn write(&self, path: &str, content: &str) -> Result<String> {
        let full = self.resolve(path)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&full, content).with_context(|| format!("writing {path}"))?;
        Ok(format!(
            "Wrote {} lines to {path}.",
            content.lines().count()
        ))
    }

    /// Apply exact-text replacements, all matched against the original content.
    ///
    /// The whole call fails if any edit does not match exactly once, so a partially applied edit can never leave the file in a state the model did not intend.
    pub fn edit(&self, path: &str, edits: &[Edit]) -> Result<String> {
        let full = self.resolve(path)?;
        let original = std::fs::read_to_string(&full).with_context(|| format!("reading {path}"))?;

        let mut updated = original.clone();
        for (i, edit) in edits.iter().enumerate() {
            match original.matches(edit.old.as_str()).count() {
                1 => {}
                0 => anyhow::bail!("edit {} did not match anything in {path}", i + 1),
                n => anyhow::bail!(
                    "edit {} matches {n} places in {path}; include more surrounding text to make it unique",
                    i + 1
                ),
            }
            updated = updated.replacen(&edit.old, &edit.new, 1);
        }

        std::fs::write(&full, &updated).with_context(|| format!("writing {path}"))?;
        Ok(format!("Applied {} edit(s) to {path}.", edits.len()))
    }
}
