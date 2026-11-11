// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::error::{self, Result};
use crate::{MetadataChange, MetadataStore};
use jiff::Timestamp;
use serde::Serialize;
use snafu::{ensure, ResultExt};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// `Datastore` persists TUF metadata files.
#[derive(Debug, Clone)]
pub(crate) struct Datastore {
    backend: Option<Arc<dyn MetadataStore>>,
    /// A lock around retrieving the datastore path.
    path_lock: Arc<RwLock<DatastorePath>>,
    /// A lock to treat the `system_time` function as a critical section.
    time_lock: Arc<Mutex<()>>,
}

impl Datastore {
    pub(crate) fn new(path: Option<PathBuf>, backend: Option<Arc<dyn MetadataStore>>) -> Result<Self> {
        Ok(Self {
            path_lock: Arc::new(RwLock::new(match (path, backend.is_some()) {
                (_, true) => DatastorePath::External,
                (None, false) => DatastorePath::TempDir(TempDir::new().context(error::DatastoreInitSnafu)?),
                (Some(p), false) => DatastorePath::Path(p),
            })),
            time_lock: Arc::new(Mutex::new(())),
            backend,
        })
    }

    async fn read(&self) -> RwLockReadGuard<'_, DatastorePath> {
        self.path_lock.read().await
    }

    async fn write(&self) -> RwLockWriteGuard<'_, DatastorePath> {
        self.path_lock.write().await
    }

    /// Get contents of a file in the datastore. This function is thread safe.
    ///
    /// TODO: [provide a thread safe interface](https://github.com/awslabs/tough/issues/602)
    ///
    pub(crate) async fn bytes(&self, file: &str) -> Result<Option<Vec<u8>>> {
        let lock = &self.read().await;
        let path = lock.path().join(file);
        if let Some(backend) = &self.backend {
            let backend = Arc::clone(backend);
            let file = file.to_owned();
            return tokio::task::spawn_blocking(move || backend.bytes(&file)).await
                .map_err(std::io::Error::other)
                .and_then(|result| result)
                .context(error::DatastoreOpenSnafu { path: &path });
        }
        match tokio::fs::read(&path).await {
            Ok(file) => Ok(Some(file)),
            Err(err) => match err.kind() {
                ErrorKind::NotFound => Ok(None),
                _ => Err(err).context(error::DatastoreOpenSnafu { path: &path }),
            },
        }
    }

    /// Writes a JSON metadata file in the datastore. This function is thread safe.
    pub(crate) async fn create<T: Serialize>(&self, file: &str, value: &T) -> Result<()> {
        self.create_with_removals(file, value, &[]).await
    }

    pub(crate) async fn create_with_removals<T: Serialize>(&self, file: &str, value: &T, removals: &[&str]) -> Result<()> {
        let lock = &self.write().await;
        let path = lock.path().join(file);
        let bytes = serde_json::to_vec(value).with_context(|_| error::DatastoreSerializeSnafu {
            what: format!("{file} in datastore"),
            path: path.clone(),
        })?;
        if let Some(backend) = &self.backend {
            let backend = Arc::clone(backend);
            let mut changes = vec![MetadataChange::Write { name: file.to_owned(), bytes }];
            changes.extend(removals.iter().map(|name| MetadataChange::Remove { name: (*name).to_owned() }));
            return tokio::task::spawn_blocking(move || backend.apply(&changes)).await
                .map_err(std::io::Error::other)
                .and_then(|result| result)
                .context(error::DatastoreCreateSnafu { path: &path });
        }
        tokio::fs::write(&path, bytes).await.context(error::DatastoreCreateSnafu { path: &path })?;
        for name in removals {
            let path = lock.path().join(name);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {},
                Err(error) if error.kind() == ErrorKind::NotFound => {},
                Err(error) => return Err(error).context(error::DatastoreRemoveSnafu { path: &path }),
            }
        }
        Ok(())
    }

    /// Ensures that system time has not stepped backward since it was last sampled. This function
    /// is protected by a lock guard to ensure thread safety.
    pub(crate) async fn system_time(&self) -> Result<Timestamp> {
        // Treat this function as a critical section. This lock is not used for anything else.
        let lock = self.time_lock.lock().await;

        let file = "latest_known_time.json";
        // Load the latest known system time, if it exists
        let poss_latest_known_time = self
            .bytes(file)
            .await?
            .map(|b| serde_json::from_slice::<Timestamp>(&b))
            .transpose().context(error::DatastoreDeserializeSnafu { path: PathBuf::from(file) })?;

        // Get 'current' system time
        let sys_time = Timestamp::now();

        if let Some(latest_known_time) = poss_latest_known_time {
            // Make sure the sampled system time did not go back in time
            ensure!(
                sys_time >= latest_known_time,
                error::SystemTimeSteppedBackwardSnafu {
                    sys_time,
                    latest_known_time
                }
            );
        }
        // Store the latest known time
        // Serializes RFC3339 time string and store to datastore
        self.create(file, &sys_time).await?;

        // Explicitly drop the lock to avoid any compiler optimization.
        drop(lock);
        Ok(sys_time)
    }
}

/// Because `TempDir` is an RAII object, we need to hold on to it. This private enum allows us to
/// hold either a `TempDir` or a `PathBuf` depending on whether or not the user wants to manage the
/// directory.
#[derive(Debug)]
enum DatastorePath {
    External,
    /// Path to a user-managed directory.
    Path(PathBuf),
    /// A `TempDir` that we created on the user's behalf.
    TempDir(TempDir),
}

impl DatastorePath {
    /// Provides convenient access to the underlying filepath.
    fn path(&self) -> &Path {
        match self {
            DatastorePath::External => Path::new("<metadata-store>"),
            DatastorePath::Path(p) => p,
            DatastorePath::TempDir(t) => t.path(),
        }
    }
}
