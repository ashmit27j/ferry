//! Hot reload from a config file (ADR 0015).
//!
//! [`watch_file`] re-reads the file every `interval` and reloads when its
//! content hash changes. Polling the content rather than subscribing to file
//! events works the same on every OS and filesystem, and survives the atomic
//! symlink swap Kubernetes uses to update a mounted ConfigMap. A file that
//! fails to parse or validate is logged once and ignored: the proxy keeps
//! serving the last good snapshot.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;

use ferry_config::{ConfigError, SnapshotVersion};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::server::ReloadHandle;

/// Default polling interval.
pub const DEFAULT_WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// What one poll did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// Content unchanged since the last poll.
    Unchanged,
    /// A new snapshot was installed with this version.
    Reloaded(SnapshotVersion),
    /// The file changed but was rejected; the message lists the problems.
    Rejected(String),
}

/// Polls one config file and installs every new valid version of it.
#[derive(Debug)]
pub struct FileWatcher {
    path: PathBuf,
    reload: ReloadHandle,
    last_hash: Option<u64>,
}

impl FileWatcher {
    /// A watcher that treats the file's current content as already loaded.
    #[must_use]
    pub fn new(path: PathBuf, reload: ReloadHandle) -> Self {
        let last_hash = std::fs::read(&path).ok().map(|b| hash(&b));
        Self {
            path,
            reload,
            last_hash,
        }
    }

    /// Checks the file once.
    pub async fn poll(&mut self) -> Poll {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(b) => b,
            Err(e) => {
                let h = hash(e.to_string().as_bytes());
                if self.last_hash == Some(h) {
                    return Poll::Unchanged;
                }
                self.last_hash = Some(h);
                let msg = format!("cannot read {}: {e}", self.path.display());
                self.reload.rejected();
                error!(config = %self.path.display(), error = %e, "config unreadable; keeping current snapshot");
                return Poll::Rejected(msg);
            }
        };
        let h = hash(&bytes);
        if self.last_hash == Some(h) {
            return Poll::Unchanged;
        }
        self.last_hash = Some(h);
        let version = SnapshotVersion(self.reload.current_version().0 + 1);
        let parsed = String::from_utf8(bytes)
            .map_err(|e| ConfigError::Parse(e.to_string()))
            .and_then(|text| ferry_config::from_yaml_str(&text))
            .and_then(|config| Ok(config.into_snapshot(version)?));
        match parsed {
            Ok(snapshot) => match self.reload.swap(snapshot) {
                Ok(()) => {
                    info!(config = %self.path.display(), %version, "config reloaded");
                    Poll::Reloaded(version)
                }
                Err(e) => {
                    error!(config = %self.path.display(), error = %e, "config rejected; keeping current snapshot");
                    Poll::Rejected(e.to_string())
                }
            },
            Err(e) => {
                self.reload.rejected();
                error!(config = %self.path.display(), error = %e, "config rejected; keeping current snapshot");
                Poll::Rejected(e.to_string())
            }
        }
    }
}

/// Polls `path` every `interval` until `token` is cancelled.
pub async fn watch_file(
    path: PathBuf,
    reload: ReloadHandle,
    interval: Duration,
    token: CancellationToken,
) {
    let mut watcher = FileWatcher::new(path, reload);
    debug!(?interval, "watching config file");
    loop {
        tokio::select! {
            () = token.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
        watcher.poll().await;
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}
