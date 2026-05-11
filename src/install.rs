//! `dyad --install`: drop a `dyad` symlink into `~/.local/bin` so the
//! editor is runnable from anywhere.
//!
//! Symlinks rather than copies: a subsequent `cargo build --release`
//! updates the installed binary without re-running `--install`. The
//! safety guard refuses to overwrite anything at the target that isn't
//! already a symlink — we never replace a real file the user wrote.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub fn install() -> Result<()> {
    let source = std::env::current_exe()
        .context("locating the current dyad binary")?
        .canonicalize()
        .context("canonicalizing the current binary path")?;

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set")?;
    let target_dir = home.join(".local/bin");
    let target = target_dir.join("dyad");

    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("creating {}", target_dir.display()))?;

    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            std::fs::remove_file(&target)
                .with_context(|| format!("removing existing symlink at {}", target.display()))?;
        } else {
            bail!(
                "refusing to overwrite {}: it exists but is not a symlink",
                target.display()
            );
        }
    }

    std::os::unix::fs::symlink(&source, &target)
        .with_context(|| format!("creating symlink at {}", target.display()))?;

    println!("Installed dyad to {}", target.display());
    println!("  -> {}", source.display());

    if !path_contains(&target_dir) {
        println!();
        println!("Note: {} is not on your PATH.", target_dir.display());
        println!("Add this to your shell profile:");
        println!("  export PATH=\"$HOME/.local/bin:$PATH\"");
    }

    Ok(())
}

fn path_contains(dir: &Path) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|p| p == dir)
}
