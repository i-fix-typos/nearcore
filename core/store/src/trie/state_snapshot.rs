use crate::db::STATE_SNAPSHOT_KEY;
use crate::flat::FlatStorageManager;
use crate::Mode;
use crate::{checkpoint_hot_storage_and_cleanup_columns, metrics, DBCol, NodeStorage};
use crate::{option_to_not_found, ShardTries};
use crate::{Store, StoreConfig};
use near_primitives::block::Block;
use near_primitives::errors::EpochError;
use near_primitives::errors::StorageError;
use near_primitives::errors::StorageError::StorageInconsistentState;
use near_primitives::hash::CryptoHash;
use near_primitives::shard_layout::ShardUId;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::TryLockError;

/// Snapshot of the state at the epoch boundary.
pub struct StateSnapshot {
    /// The state snapshot represents the state including changes of the next block of this block.
    prev_block_hash: CryptoHash,
    /// Read-only store.
    store: Store,
    /// Access to flat storage in that store.
    flat_storage_manager: FlatStorageManager,
}

impl StateSnapshot {
    /// Creates an object and also creates flat storage for the given shards.
    pub fn new(
        store: Store,
        prev_block_hash: CryptoHash,
        flat_storage_manager: FlatStorageManager,
        shard_uids: &[ShardUId],
        block: Option<&Block>,
    ) -> Self {
        tracing::debug!(target: "state_snapshot", ?shard_uids, ?prev_block_hash, "new StateSnapshot");
        for shard_uid in shard_uids {
            if let Err(err) = flat_storage_manager.create_flat_storage_for_shard(*shard_uid) {
                tracing::warn!(target: "state_snapshot", ?err, ?shard_uid, "Failed to create a flat storage for snapshot shard");
                continue;
            }
            if let Some(block) = block {
                let flat_storage =
                    flat_storage_manager.get_flat_storage_for_shard(*shard_uid).unwrap();
                let current_flat_head = flat_storage.get_head_hash();
                tracing::debug!(target: "state_snapshot", ?shard_uid, ?current_flat_head, block_hash = ?block.header().hash(), block_height = block.header().height(), "Moving FlatStorage head of the snapshot");
                let _timer = metrics::MOVE_STATE_SNAPSHOT_FLAT_HEAD_ELAPSED
                    .with_label_values(&[&shard_uid.shard_id.to_string()])
                    .start_timer();
                if let Some(chunk) = block.chunks().get(shard_uid.shard_id as usize) {
                    // Flat state snapshot needs to be at a height that lets it
                    // replay the last chunk of the shard.
                    let desired_flat_head = chunk.prev_block_hash();
                    match flat_storage.update_flat_head(desired_flat_head, true) {
                        Ok(_) => {
                            tracing::debug!(target: "state_snapshot", ?shard_uid, ?current_flat_head, ?desired_flat_head, "Successfully moved FlatStorage head of the snapshot");
                        }
                        Err(err) => {
                            tracing::error!(target: "state_snapshot", ?shard_uid, ?err, ?current_flat_head, ?desired_flat_head, "Failed to move FlatStorage head of the snapshot");
                        }
                    }
                } else {
                    tracing::error!(target: "state_snapshot", ?shard_uid, current_flat_head = ?flat_storage.get_head_hash(), ?prev_block_hash, "Failed to move FlatStorage head of the snapshot, no chunk");
                }
            }
        }
        Self { prev_block_hash, store, flat_storage_manager }
    }
}

/// Information needed to make a state snapshot.
#[derive(Debug)]
pub enum StateSnapshotConfig {
    /// Don't make any state snapshots.
    Disabled,
    Enabled {
        home_dir: PathBuf,
        hot_store_path: PathBuf,
        state_snapshot_subdir: PathBuf,
        compaction_enabled: bool,
    },
}

impl ShardTries {
    pub fn get_state_snapshot(
        &self,
        block_hash: &CryptoHash,
    ) -> Result<(Store, FlatStorageManager), StorageError> {
        // Taking this lock can last up to 10 seconds, if the snapshot happens to be re-created.
        match self.state_snapshot().try_read() {
            Ok(guard) => {
                if let Some(data) = guard.as_ref() {
                    if &data.prev_block_hash != block_hash {
                        return Err(StorageInconsistentState(format!(
                            "Wrong state snapshot. Requested: {:?}, Available: {:?}",
                            block_hash, data.prev_block_hash
                        )));
                    }
                    Ok((data.store.clone(), data.flat_storage_manager.clone()))
                } else {
                    Err(StorageInconsistentState("No state snapshot available".to_string()))
                }
            }
            Err(TryLockError::WouldBlock) => Err(StorageInconsistentState(
                "Accessing state snapshot would block. Retry in a few seconds.".to_string(),
            )),
            Err(err) => {
                Err(StorageInconsistentState(format!("Can't access state snapshot: {err:?}")))
            }
        }
    }

    /// Makes a snapshot of the current state of the DB.
    /// If a snapshot was previously available, it gets deleted.
    pub fn make_state_snapshot(
        &self,
        prev_block_hash: &CryptoHash,
        shard_uids: &[ShardUId],
        block: &Block,
    ) -> Result<(), anyhow::Error> {
        metrics::HAS_STATE_SNAPSHOT.set(0);
        // The function returns an `anyhow::Error`, because no special handling of errors is done yet. The errors are logged and ignored.
        let _span =
            tracing::info_span!(target: "state_snapshot", "make_state_snapshot", ?prev_block_hash)
                .entered();
        tracing::info!(target: "state_snapshot", ?prev_block_hash, "make_state_snapshot");
        match &self.state_snapshot_config() {
            StateSnapshotConfig::Disabled => {
                tracing::info!(target: "state_snapshot", "State Snapshots are disabled");
                Ok(())
            }
            StateSnapshotConfig::Enabled {
                home_dir,
                hot_store_path,
                state_snapshot_subdir,
                compaction_enabled: _,
            } => {
                let _timer = metrics::MAKE_STATE_SNAPSHOT_ELAPSED.start_timer();
                // `write()` lock is held for the whole duration of this function.
                // Accessing the snapshot in other parts of the system will fail.
                let mut state_snapshot_lock = self.state_snapshot().write().map_err(|_| {
                    anyhow::Error::msg("error accessing write lock of state_snapshot")
                })?;
                let db_snapshot_hash = self.get_state_snapshot_hash();

                if let Some(state_snapshot) = &*state_snapshot_lock {
                    // only return Ok() when the hash stored in STATE_SNAPSHOT_KEY and in state_snapshot_lock and prev_block_hash are the same
                    if db_snapshot_hash.is_ok()
                        && db_snapshot_hash.unwrap() == *prev_block_hash
                        && state_snapshot.prev_block_hash == *prev_block_hash
                    {
                        tracing::warn!(target: "state_snapshot", ?prev_block_hash, "Requested a state snapshot but that is already available");
                        return Ok(());
                    } else {
                        // Drop Store before deleting the underlying data.
                        *state_snapshot_lock = None;

                        // This will delete all existing snapshots from file system. If failed, will retry until success
                        let mut delete_state_snapshots_from_file_system = false;
                        let mut file_system_delete_retries = 0;
                        while !delete_state_snapshots_from_file_system
                            && file_system_delete_retries < 3
                        {
                            delete_state_snapshots_from_file_system = self
                                .delete_all_state_snapshots(
                                    home_dir,
                                    hot_store_path,
                                    state_snapshot_subdir,
                                );
                            file_system_delete_retries += 1;
                        }

                        // this will delete the STATE_SNAPSHOT_KEY-value pair from db. If failed, will retry until success
                        let mut delete_state_snapshot_from_db = false;
                        let mut db_delete_retries = 0;
                        while !delete_state_snapshot_from_db && db_delete_retries < 3 {
                            delete_state_snapshot_from_db = match self.set_state_snapshot_hash(None)
                            {
                                Ok(_) => true,
                                Err(err) => {
                                    // This will be retried.
                                    tracing::debug!(target: "state_snapshot", ?err, "Failed to delete the old state snapshot for BlockMisc::STATE_SNAPSHOT_KEY in rocksdb");
                                    false
                                }
                            };
                            db_delete_retries += 1;
                        }

                        metrics::HAS_STATE_SNAPSHOT.set(0);
                    }
                }

                let storage = checkpoint_hot_storage_and_cleanup_columns(
                    &self.get_store(),
                    &Self::get_state_snapshot_base_dir(
                        prev_block_hash,
                        home_dir,
                        hot_store_path,
                        state_snapshot_subdir,
                    ),
                    // TODO: Cleanup Changes and DeltaMetadata to avoid extra memory usage.
                    // Can't be cleaned up now because these columns are needed to `update_flat_head()`.
                    Some(vec![
                        // Keep DbVersion and BlockMisc, otherwise you'll not be able to open the state snapshot as a Store.
                        DBCol::DbVersion,
                        DBCol::BlockMisc,
                        // Flat storage columns.
                        DBCol::FlatState,
                        DBCol::FlatStateChanges,
                        DBCol::FlatStateDeltaMetadata,
                        DBCol::FlatStorageStatus,
                    ]),
                )?;
                let store = storage.get_hot_store();
                // It is fine to create a separate FlatStorageManager, because
                // it is used only for reading flat storage in the snapshot a
                // doesn't introduce memory overhead.
                let flat_storage_manager = FlatStorageManager::new(store.clone());
                *state_snapshot_lock = Some(StateSnapshot::new(
                    store,
                    *prev_block_hash,
                    flat_storage_manager,
                    shard_uids,
                    Some(block),
                ));

                // this will set the new hash for state snapshot in rocksdb. will retry until success.
                let mut set_state_snapshot_in_db = false;
                while !set_state_snapshot_in_db {
                    set_state_snapshot_in_db = match self
                        .set_state_snapshot_hash(Some(*prev_block_hash))
                    {
                        Ok(_) => true,
                        Err(err) => {
                            // This will be retried.
                            tracing::debug!(target: "state_snapshot", ?err, "Failed to set the new state snapshot for BlockMisc::STATE_SNAPSHOT_KEY in rocksdb");
                            false
                        }
                    }
                }

                metrics::HAS_STATE_SNAPSHOT.set(1);
                tracing::info!(target: "state_snapshot", ?prev_block_hash, "Made a checkpoint");
                Ok(())
            }
        }
    }

    /// Runs compaction on the snapshot.
    pub fn compact_state_snapshot(&self) -> Result<(), anyhow::Error> {
        let _span =
            tracing::info_span!(target: "state_snapshot", "compact_state_snapshot").entered();
        // It's fine if the access to state snapshot blocks.
        let state_snapshot_lock = self
            .state_snapshot()
            .read()
            .map_err(|_| anyhow::Error::msg("error accessing read lock of state_snapshot"))?;
        if let Some(state_snapshot) = &*state_snapshot_lock {
            let _timer = metrics::COMPACT_STATE_SNAPSHOT_ELAPSED.start_timer();
            Ok(state_snapshot.store.compact()?)
        } else {
            tracing::warn!(target: "state_snapshot", "Requested compaction but no state snapshot is available.");
            Ok(())
        }
    }

    /// Deletes all existing state snapshots in the parent directory
    fn delete_all_state_snapshots(
        &self,
        home_dir: &Path,
        hot_store_path: &Path,
        state_snapshot_subdir: &Path,
    ) -> bool {
        let _timer = metrics::DELETE_STATE_SNAPSHOT_ELAPSED.start_timer();
        let _span =
            tracing::info_span!(target: "state_snapshot", "delete_state_snapshot").entered();
        let path = home_dir.join(hot_store_path).join(state_snapshot_subdir);
        match std::fs::remove_dir_all(&path) {
            Ok(_) => {
                tracing::info!(target: "state_snapshot", ?path, "Deleted all state snapshots");
                true
            }
            Err(err) => {
                tracing::warn!(target: "state_snapshot", ?err, ?path, "Failed to delete all state snapshots");
                false
            }
        }
    }

    pub fn get_state_snapshot_base_dir(
        prev_block_hash: &CryptoHash,
        home_dir: &Path,
        hot_store_path: &Path,
        state_snapshot_subdir: &Path,
    ) -> PathBuf {
        // Assumptions:
        // * RocksDB checkpoints are taken instantly and for free, because the filesystem supports hard links.
        // * The best place for checkpoints is within the `hot_store_path`, because that directory is often a separate disk.
        home_dir.join(hot_store_path).join(state_snapshot_subdir).join(format!("{prev_block_hash}"))
    }

    /// Retrieves STATE_SNAPSHOT_KEY
    pub fn get_state_snapshot_hash(&self) -> Result<CryptoHash, io::Error> {
        option_to_not_found(
            self.get_store().get_ser(DBCol::BlockMisc, STATE_SNAPSHOT_KEY),
            "STATE_SNAPSHOT_KEY",
        )
    }

    /// Updates STATE_SNAPSHOT_KEY.
    pub fn set_state_snapshot_hash(&self, value: Option<CryptoHash>) -> Result<(), io::Error> {
        let mut store_update = self.store_update();
        let key = STATE_SNAPSHOT_KEY;
        match value {
            None => store_update.delete(DBCol::BlockMisc, key),
            Some(value) => store_update.set_ser(DBCol::BlockMisc, key, &value)?,
        }
        store_update.commit().map_err(|err| err.into())
    }

    /// Read RocksDB for the latest available snapshot hash, if available, open base_path+snapshot_hash for the state snapshot
    /// we don't deal with multiple snapshots here because we will deal with it whenever a new snapshot is created and saved to file system
    pub fn maybe_open_state_snapshot(
        &self,
        get_shard_uids_fn: impl Fn(CryptoHash) -> Result<Vec<ShardUId>, EpochError>,
    ) -> Result<(), anyhow::Error> {
        let _span =
            tracing::info_span!(target: "state_snapshot", "maybe_open_state_snapshot").entered();
        metrics::HAS_STATE_SNAPSHOT.set(0);
        match &self.state_snapshot_config() {
            StateSnapshotConfig::Disabled => {
                tracing::debug!(target: "state_snapshot", "Disabled");
                return Ok(());
            }
            StateSnapshotConfig::Enabled {
                home_dir,
                hot_store_path,
                state_snapshot_subdir,
                compaction_enabled: _,
            } => {
                // directly return error if no snapshot is found
                let snapshot_hash: CryptoHash = self.get_state_snapshot_hash()?;

                let snapshot_path = Self::get_state_snapshot_base_dir(
                    &snapshot_hash,
                    &home_dir,
                    &hot_store_path,
                    &state_snapshot_subdir,
                );
                let parent_path = snapshot_path
                    .parent()
                    .ok_or(anyhow::anyhow!("{snapshot_path:?} needs to have a parent dir"))?;
                tracing::debug!(target: "state_snapshot", ?snapshot_path, ?parent_path);

                let store_config = StoreConfig::default();

                let opener = NodeStorage::opener(&snapshot_path, false, &store_config, None);
                let storage = opener.open_in_mode(Mode::ReadOnly)?;
                let store = storage.get_hot_store();
                let flat_storage_manager = FlatStorageManager::new(store.clone());

                let shard_uids = get_shard_uids_fn(snapshot_hash)?;
                let mut guard = self.state_snapshot().write().map_err(|_| {
                    anyhow::Error::msg("error accessing write lock of state_snapshot")
                })?;
                *guard = Some(StateSnapshot::new(
                    store,
                    snapshot_hash,
                    flat_storage_manager,
                    &shard_uids,
                    None,
                ));
                metrics::HAS_STATE_SNAPSHOT.set(1);
                tracing::info!(target: "runtime", ?snapshot_hash, ?snapshot_path, "Detected and opened a state snapshot.");
                Ok(())
            }
        }
    }
}
