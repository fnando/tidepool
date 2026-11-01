//! The daemon's startup check: the config file (`--config`, or the one last
//! imported) must match the stored config. It never imports on its own: a
//! difference stops it, unless `--on-config-mismatch import|export` says
//! which side wins.

use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::app::{App, config_changes};
use crate::cli::ConfigMismatch;
use crate::config::load_str;
use crate::config::store::{ConfigStore, Imported, hash};
use crate::state::State;

pub(crate) fn check(
    app: &App<'_>,
    state: &State,
    mismatch: Option<ConfigMismatch>,
) -> anyhow::Result<()> {
    let store = ConfigStore::new(&state.conn);
    let current = store.current()?;
    let path: PathBuf = match &app.cli.config {
        Some(path) => path.clone(),
        None => match store
            .last_import()?
            .and_then(|r| r.source_path)
            .map(PathBuf::from)
            .filter(|p| p.exists())
        {
            Some(path) => path,
            None => app.dirs.default_config_file(),
        },
    };
    let name = App::display_path(&path);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) if app.cli.config.is_none() => {
            tracing::debug!("no config file at {name} to compare with");
            return Ok(());
        }
        Err(e) => bail!("cannot read {name}: {e}"),
    };
    let Some(current) = current else {
        // Nothing stored yet: only importing makes sense.
        return match mismatch {
            Some(ConfigMismatch::Import) => import(&store, &name, &text),
            _ => bail!(
                "tidepool has no stored config yet; run `tidepool config import {name}` \
                 (or start with --on-config-mismatch import)"
            ),
        };
    };
    if hash(&text) == current.source_hash {
        return Ok(());
    }
    let loaded = match load_str(&name, &text) {
        Ok(loaded) => loaded,
        Err(err) if mismatch == Some(ConfigMismatch::Export) => {
            tracing::debug!("{name} has errors, replaced by the stored config: {err}");
            return export(&path, &name, &current.toml, current.rev);
        }
        Err(err) => bail!(
            "{name} has errors, so it can't be compared with the stored config \
             (revision {}):\n{err}\nFix it, or start with --on-config-mismatch export to \
             replace it with the stored config.",
            current.rev
        ),
    };
    if loaded.normalized() == current.json {
        return Ok(()); // only comments or formatting differ
    }
    match mismatch {
        Some(ConfigMismatch::Import) => import(&store, &name, &text),
        Some(ConfigMismatch::Export) => export(&path, &name, &current.toml, current.rev),
        Some(ConfigMismatch::Fail) | None => {
            let changes = config_changes(&current.json, loaded.json)?;
            bail!(
                "{name} differs from the stored config (revision {}, from {}):\n{}\n\
                 To use the file, run `tidepool config import {name}`; to keep the stored \
                 config, write it to the file with `tidepool config export --show-secrets > \
                 {name}`. Or start with --on-config-mismatch import (or export) to do that \
                 automatically.",
                current.rev,
                current.source,
                changes.join("\n")
            )
        }
    }
}

fn import(store: &ConfigStore<'_>, name: &str, text: &str) -> anyhow::Result<()> {
    let loaded = load_str(name, text)?;
    match store.import(Some(name), text, &loaded)? {
        Imported::Created(rev) => tracing::info!("imported {name} as config revision {rev}"),
        Imported::Unchanged(rev) => tracing::debug!("{name} matches config revision {rev}"),
    }
    Ok(())
}

/// Write the stored config to the file, keeping the old one as `<file>.bak`
/// (and its permissions: it may hold an API key).
fn export(path: &Path, name: &str, toml: &str, rev: i64) -> anyhow::Result<()> {
    let mut backup = path.as_os_str().to_owned();
    backup.push(".bak");
    let backup = PathBuf::from(backup);
    let permissions = std::fs::metadata(path).map(|m| m.permissions()).ok();
    std::fs::copy(path, &backup).map_err(|e| anyhow::anyhow!("cannot back up {name}: {e}"))?;
    let mut staged = path.as_os_str().to_owned();
    staged.push(".tidepool-partial");
    let staged = PathBuf::from(staged);
    std::fs::write(&staged, toml).map_err(|e| anyhow::anyhow!("cannot write {name}: {e}"))?;
    if let Some(permissions) = permissions {
        std::fs::set_permissions(&staged, permissions)?;
    }
    std::fs::rename(&staged, path).map_err(|e| anyhow::anyhow!("cannot write {name}: {e}"))?;
    tracing::info!(
        "wrote config revision {rev} to {name} (the old file is {})",
        App::display_path(&backup)
    );
    Ok(())
}
