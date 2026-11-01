//! What the web UI and its JSON API show, read from the state DB.

use std::collections::BTreeMap;

use rusqlite::Connection;
use serde::Serialize;

use crate::config::Config;
use crate::config::merge::effective_sources;
use crate::daemon::service::{Service, local};
use crate::download::human_bytes;
use crate::state::daemon as store;

/// Everything on the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub services: Vec<ServiceInfo>,
    /// When the syncer syncs next (local time), if it's on a schedule.
    pub next_sync: Option<String>,
    pub last_sync: Option<String>,
    pub sources: Vec<SourceInfo>,
    pub problems: Vec<ProblemInfo>,
    /// Downloads waiting to be finished.
    pub waiting_downloads: i64,
    /// `finished` hooks waiting to run (or to be tried again).
    pub waiting_hooks: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceInfo {
    pub name: String,
    pub status: String,
    pub activity: Option<String>,
    pub paused: bool,
    pub restarts: i64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub name: String,
    pub title: Option<String>,
    pub category: String,
    pub enabled: bool,
    pub last_indexed: Option<String>,
    /// Videos per group: downloaded, waiting, skipped, failed, unavailable,
    /// cleaned_up.
    pub counts: BTreeMap<String, i64>,
    pub disk_bytes: u64,
    pub disk: String,
    /// `cleanup.max_size`, if set.
    pub max_size: Option<String>,
}

impl SourceInfo {
    pub fn count(&self, group: &str) -> i64 {
        self.counts.get(group).copied().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProblemInfo {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub text: String,
}

/// The groups a video's status falls in, as the dashboard counts them.
pub fn group(status: &str) -> &'static str {
    match status {
        "done" => "downloaded",
        "pending" | "pending_check" | "downloading" | "fetched" | "processing" => "waiting",
        "filtered" | "filter_error" => "skipped",
        "failed" => "failed",
        "unavailable" => "unavailable",
        "cleaned_up" => "cleaned_up",
        _ => "other",
    }
}

pub fn status(conn: &Connection, config: &Config) -> rusqlite::Result<Status> {
    let services = Service::ALL
        .into_iter()
        .map(|service| {
            let row = store::service(conn, service.name())?;
            Ok(ServiceInfo {
                name: service.name().to_string(),
                status: if row.paused {
                    "paused".into()
                } else {
                    row.status.unwrap_or_else(|| "not running".into())
                },
                activity: row.activity,
                paused: row.paused,
                restarts: row.restarts,
                last_error: row.last_error,
            })
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next_sync = config
        .schedule
        .interval
        .0
        .and_then(|_| store::get_time(conn, store::NEXT_SYNC_AT).ok().flatten())
        .map(local);
    let last_sync = store::get_time(conn, store::LAST_SYNC_FINISHED_AT)?.map(local);

    let mut counts: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let mut disk: BTreeMap<String, u64> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT source_key, status, COUNT(*), SUM(COALESCE(media_bytes, 0)) FROM items
             GROUP BY source_key, status",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (source, status, n, bytes) = row?;
            *counts
                .entry(source.clone())
                .or_default()
                .entry(group(&status).to_string())
                .or_default() += n;
            if status == "done" {
                *disk.entry(source).or_default() += bytes.max(0) as u64;
            }
        }
    }
    let mut sources = Vec::new();
    for source in effective_sources(config) {
        let (title, last_indexed): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT title, last_indexed_at FROM sources WHERE key = ?1",
                [&source.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((None, None));
        let bytes = disk.get(&source.key).copied().unwrap_or(0);
        sources.push(SourceInfo {
            title,
            category: source.category.clone(),
            enabled: source.enabled,
            last_indexed: last_indexed.and_then(|t| t.parse().ok()).map(local),
            counts: counts.remove(&source.key).unwrap_or_default(),
            disk_bytes: bytes,
            disk: human_bytes(bytes),
            max_size: source.cleanup.max_size.bytes.map(human_bytes),
            name: source.key,
        });
    }

    let problems = crate::problems::open(conn)?
        .into_iter()
        .map(|p| ProblemInfo {
            id: p.id,
            kind: p.kind.as_str().to_string(),
            title: p.kind.title().to_string(),
            text: crate::problems::describe(&p),
        })
        .collect();
    let count = |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |r| r.get(0)) };
    Ok(Status {
        services,
        next_sync,
        last_sync,
        sources,
        problems,
        waiting_downloads: count(
            "SELECT COUNT(*) FROM items WHERE status IN ('fetched', 'processing')",
        )?,
        waiting_hooks: count(
            "SELECT COUNT(*) FROM hook_runs WHERE type = 'finished' AND status IN ('pending', 'retrying')",
        )?,
    })
}
