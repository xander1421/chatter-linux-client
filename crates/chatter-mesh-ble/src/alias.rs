//! Persist the adapter's alias before masking it so an interrupted session can
//! undo the change on the next app start.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bluer::{Adapter, Session};
use chatter_mesh_link::LinkError;

const MESH_ALIAS: &str = "Bluetooth Device";

fn alias_dir() -> Option<PathBuf> {
    directories::BaseDirs::new()?
        .state_dir()
        .map(|path| path.join("chatter").join("mesh-aliases"))
}

fn alias_error(error: impl std::fmt::Display) -> LinkError {
    LinkError::PeripheralSetup(format!(
        "could not protect the Bluetooth adapter name: {error}"
    ))
}

struct SavedAlias {
    name: String,
    system_fallback: bool,
}

struct AliasJournal {
    path: PathBuf,
    _lock: File,
}

impl AliasJournal {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path.with_extension("lock"))?;
        lock.try_lock().map_err(io::Error::from)?;
        Ok(Self { path, _lock: lock })
    }

    fn load(&self) -> io::Result<Option<SavedAlias>> {
        let record = match fs::read_to_string(&self.path) {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some((kind @ ("alias" | "system"), name)) = record.split_once('\n') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid saved adapter alias",
            ));
        };
        Ok(Some(SavedAlias {
            name: name.to_owned(),
            system_fallback: kind == "system",
        }))
    }

    fn save(&self, directory: &Path, saved: &SavedAlias) -> io::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        let kind = if saved.system_fallback {
            "system"
        } else {
            "alias"
        };
        writeln!(file, "{kind}")?;
        file.write_all(saved.name.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        File::open(directory)?.sync_all()
    }

    async fn restore(&self, adapter: &Adapter) -> Result<(), LinkError> {
        let Some(saved) = self.load().map_err(alias_error)? else {
            return Ok(());
        };

        // Preserve an unrelated rename made by the user while mesh was on.
        // The original value can also be visible while BlueZ processes SetAlias.
        let current = adapter.alias().await.map_err(alias_error)?;
        if current == MESH_ALIAS || current == saved.name {
            let (alias, expected) = if saved.system_fallback {
                (
                    String::new(),
                    adapter.system_name().await.map_err(alias_error)?,
                )
            } else {
                (saved.name.clone(), saved.name)
            };
            set_checked_alias(adapter, alias, &expected).await?;
        }
        fs::remove_file(&self.path).map_err(alias_error)
    }
}

async fn journal_path(directory: &Path, adapter: &Adapter) -> Result<PathBuf, LinkError> {
    let address = adapter.address().await.map_err(alias_error)?;
    Ok(directory.join(format!("{address}.alias")))
}

async fn set_checked_alias(
    adapter: &Adapter,
    requested: String,
    expected: &str,
) -> Result<(), LinkError> {
    // The D-Bus write is accepted before the controller reports the change.
    tokio::time::timeout(Duration::from_secs(2), async {
        adapter.set_alias(requested).await.map_err(alias_error)?;
        loop {
            if adapter.alias().await.map_err(alias_error)? == expected {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| alias_error("BlueZ did not report the requested alias"))?
}

/// A per-adapter lease held until the radio shuts down.
pub(crate) struct MeshAlias {
    adapter: Adapter,
    journal: Option<AliasJournal>,
    directory: PathBuf,
}

impl MeshAlias {
    pub(crate) async fn acquire(adapter: &Adapter) -> Result<Self, LinkError> {
        let directory = alias_dir().ok_or_else(|| alias_error("no local state directory"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(alias_error)?;
        let path = journal_path(&directory, adapter).await?;
        let journal = AliasJournal::acquire(path).map_err(alias_error)?;
        let lease = Self {
            adapter: adapter.clone(),
            journal: Some(journal),
            directory,
        };
        lease.restore().await?;
        Ok(lease)
    }

    pub(crate) async fn install(&self) -> Result<(), LinkError> {
        let journal = self
            .journal
            .as_ref()
            .ok_or_else(|| alias_error("alias lease ended"))?;
        let name = self.adapter.alias().await.map_err(alias_error)?;
        if name != MESH_ALIAS {
            let system_fallback = name == self.adapter.system_name().await.map_err(alias_error)?;
            journal
                .save(
                    &self.directory,
                    &SavedAlias {
                        name,
                        system_fallback,
                    },
                )
                .map_err(alias_error)?;
        }
        set_checked_alias(&self.adapter, MESH_ALIAS.to_owned(), MESH_ALIAS).await
    }

    pub(crate) async fn restore(&self) -> Result<(), LinkError> {
        if let Some(journal) = &self.journal {
            journal.restore(&self.adapter).await?;
        }
        Ok(())
    }
}

impl Drop for MeshAlias {
    fn drop(&mut self) {
        if let Some(journal) = self.journal.take()
            && journal.path.is_file()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let adapter = self.adapter.clone();
            runtime.spawn(async move {
                if let Err(error) = journal.restore(&adapter).await {
                    tracing::warn!(%error, "could not restore the Bluetooth adapter alias");
                }
            });
        }
    }
}

/// Recover interrupted alias changes even if Mesh is disabled on this launch.
pub async fn recover_desktop_aliases() -> Result<(), LinkError> {
    let Some(directory) = alias_dir() else {
        return Ok(());
    };
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(alias_error(error)),
    };
    if !entries.filter_map(Result::ok).any(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|suffix| suffix == "alias")
    }) {
        return Ok(());
    }

    let session = Session::new().await.map_err(alias_error)?;
    for name in session.adapter_names().await.map_err(alias_error)? {
        let adapter = session.adapter(&name).map_err(alias_error)?;
        let path = journal_path(&directory, &adapter).await?;
        if !path.is_file() {
            continue;
        }
        match AliasJournal::acquire(path) {
            Ok(journal) => {
                if let Err(error) = journal.restore(&adapter).await {
                    tracing::warn!(%error, "could not recover the saved Bluetooth adapter alias");
                }
            }
            // Another running session owns the alias.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(alias_error(error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_alias_survives_the_lease_and_is_exclusive() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("adapter.alias");
        let journal = AliasJournal::acquire(path.clone()).unwrap();
        for system_fallback in [false, true] {
            journal
                .save(
                    directory.path(),
                    &SavedAlias {
                        name: "Previous device name".into(),
                        system_fallback,
                    },
                )
                .unwrap();
            let saved = journal.load().unwrap().unwrap();
            assert_eq!(saved.name, "Previous device name");
            assert_eq!(saved.system_fallback, system_fallback);
        }
        assert!(matches!(AliasJournal::acquire(path.clone()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock));
        drop(journal);

        let recovered = AliasJournal::acquire(path)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(recovered.name, "Previous device name");
        assert!(recovered.system_fallback);
    }
}
