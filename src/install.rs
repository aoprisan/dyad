//! `dyad --install`: copy the `dyad` binary into `~/.local/bin` so the
//! editor is runnable from anywhere.
//!
//! Copies rather than symlinks: the installed binary is a standalone
//! snapshot that keeps working after the build directory is cleaned or
//! moved. The trade-off is that a later `cargo build --release` no longer
//! updates the installed copy in place — re-run `--install` to refresh
//! it. We refuse to overwrite a directory at the target, but a prior
//! install (a copy, or an old-style symlink from before this change) is
//! replaced.

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
        let file_type = meta.file_type();
        if file_type.is_dir() {
            bail!(
                "refusing to overwrite {}: it is a directory",
                target.display()
            );
        }
        // A regular file whose canonical path is the source is the running
        // binary itself (`dyad --install` from the installed copy) — nothing
        // to do. Symlinks always fall through so an old-style install is
        // migrated to a copy.
        if !file_type.is_symlink() && std::fs::canonicalize(&target).ok() == Some(source.clone()) {
            println!("dyad is already installed at {}", target.display());
            return Ok(());
        }
        // Existing copy or old symlink — remove it so the copy starts clean.
        std::fs::remove_file(&target)
            .with_context(|| format!("removing existing file at {}", target.display()))?;
    }

    std::fs::copy(&source, &target)
        .with_context(|| format!("copying binary to {}", target.display()))?;

    println!("Installed dyad to {}", target.display());
    println!("  copied from {}", source.display());

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
