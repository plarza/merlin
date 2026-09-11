use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

pub struct Workspace {
    root: PathBuf,
}

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

    pub fn save_incoming(&self, filename: &str, bytes: &[u8]) -> Result<String> {
        let safe: String = filename
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let safe = if safe.trim_matches('_').is_empty() {
            "attachment".to_string()
        } else {
            safe
        };

        let relative = format!("inbox/{safe}");
        let full = self.resolve(&relative)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&full, bytes).with_context(|| format!("writing {relative}"))?;
        Ok(relative)
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
