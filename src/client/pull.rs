//! Sync datastore from remote server

use anyhow::{bail, format_err, Error};
use serde_json::json;
use std::convert::TryFrom;
use std::sync::Arc;
use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use chrono::{Utc, TimeZone};

use crate::server::{WorkerTask};
use crate::backup::*;
use crate::api2::types::*;
use super::*;


// fixme: implement filters
// fixme: delete vanished groups
// Todo: correctly lock backup groups

async fn pull_index_chunks<I: IndexFile>(
    _worker: &WorkerTask,
    chunk_reader: &mut RemoteChunkReader,
    target: Arc<DataStore>,
    index: I,
) -> Result<(), Error> {


    for pos in 0..index.index_count() {
        let digest = index.index_digest(pos).unwrap();
        let chunk_exists = target.cond_touch_chunk(digest, false)?;
        if chunk_exists {
            //worker.log(format!("chunk {} exists {}", pos, proxmox::tools::digest_to_hex(digest)));
            continue;
        }
        //worker.log(format!("sync {} chunk {}", pos, proxmox::tools::digest_to_hex(digest)));
        let chunk = chunk_reader.read_raw_chunk(&digest).await?;

        target.insert_chunk(&chunk, &digest)?;
    }

    Ok(())
}

async fn download_manifest(
    reader: &BackupReader,
    filename: &std::path::Path,
) -> Result<std::fs::File, Error> {

    let mut tmp_manifest_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .read(true)
        .open(&filename)?;

    reader.download(MANIFEST_BLOB_NAME, &mut tmp_manifest_file).await?;

    tmp_manifest_file.seek(SeekFrom::Start(0))?;

    Ok(tmp_manifest_file)
}

async fn pull_single_archive(
    worker: &WorkerTask,
    reader: &BackupReader,
    chunk_reader: &mut RemoteChunkReader,
    tgt_store: Arc<DataStore>,
    snapshot: &BackupDir,
    archive_name: &str,
) -> Result<(), Error> {

    let mut path = tgt_store.base_path();
    path.push(snapshot.relative_path());
    path.push(archive_name);

    let mut tmp_path = path.clone();
    tmp_path.set_extension("tmp");

    worker.log(format!("sync archive {}", archive_name));
    let mut tmpfile = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .read(true)
        .open(&tmp_path)?;

    reader.download(archive_name, &mut tmpfile).await?;

    match archive_type(archive_name)? {
        ArchiveType::DynamicIndex => {
            let index = DynamicIndexReader::new(tmpfile)
                .map_err(|err| format_err!("unable to read dynamic index {:?} - {}", tmp_path, err))?;

            pull_index_chunks(worker, chunk_reader, tgt_store.clone(), index).await?;
        }
        ArchiveType::FixedIndex => {
            let index = FixedIndexReader::new(tmpfile)
                .map_err(|err| format_err!("unable to read fixed index '{:?}' - {}", tmp_path, err))?;

            pull_index_chunks(worker, chunk_reader, tgt_store.clone(), index).await?;
        }
        ArchiveType::Blob => { /* nothing to do */ }
    }
    if let Err(err) = std::fs::rename(&tmp_path, &path) {
        bail!("Atomic rename file {:?} failed - {}", path, err);
    }
    Ok(())
}

// Note: The client.log.blob is uploaded after the backup, so it is
// not mentioned in the manifest.
async fn try_client_log_download(
    worker: &WorkerTask,
    reader: Arc<BackupReader>,
    path: &std::path::Path,
) -> Result<(), Error> {

    let mut tmp_path = path.to_owned();
    tmp_path.set_extension("tmp");

    let tmpfile = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .read(true)
        .open(&tmp_path)?;

    // Note: be silent if there is no log - only log successful download
    if let Ok(()) = reader.download(CLIENT_LOG_BLOB_NAME, tmpfile).await {
        if let Err(err) = std::fs::rename(&tmp_path, &path) {
            bail!("Atomic rename file {:?} failed - {}", path, err);
        }
        worker.log(format!("got backup log file {:?}", CLIENT_LOG_BLOB_NAME));
    }

    Ok(())
}

async fn pull_snapshot(
    worker: &WorkerTask,
    reader: Arc<BackupReader>,
    tgt_store: Arc<DataStore>,
    snapshot: &BackupDir,
) -> Result<(), Error> {

    let mut manifest_name = tgt_store.base_path();
    manifest_name.push(snapshot.relative_path());
    manifest_name.push(MANIFEST_BLOB_NAME);

    let mut client_log_name = tgt_store.base_path();
    client_log_name.push(snapshot.relative_path());
    client_log_name.push(CLIENT_LOG_BLOB_NAME);

    let mut tmp_manifest_name = manifest_name.clone();
    tmp_manifest_name.set_extension("tmp");

    let mut tmp_manifest_file = download_manifest(&reader, &tmp_manifest_name).await?;
    let tmp_manifest_blob = DataBlob::load(&mut tmp_manifest_file)?;
    tmp_manifest_blob.verify_crc()?;

    if manifest_name.exists() {
        let manifest_blob = proxmox::try_block!({
            let mut manifest_file = std::fs::File::open(&manifest_name)
                .map_err(|err| format_err!("unable to open local manifest {:?} - {}", manifest_name, err))?;

            let manifest_blob = DataBlob::load(&mut manifest_file)?;
            manifest_blob.verify_crc()?;
            Ok(manifest_blob)
        }).map_err(|err: Error| {
            format_err!("unable to read local manifest {:?} - {}", manifest_name, err)
        })?;

        if manifest_blob.raw_data() == tmp_manifest_blob.raw_data() {
            if !client_log_name.exists() {
                try_client_log_download(worker, reader, &client_log_name).await?;
            }
            worker.log("no data changes");
            return Ok(()); // nothing changed
        }
    }

    let manifest = BackupManifest::try_from(tmp_manifest_blob)?;

    let mut chunk_reader = RemoteChunkReader::new(reader.clone(), None, HashMap::new());

    for item in manifest.files() {
        let mut path = tgt_store.base_path();
        path.push(snapshot.relative_path());
        path.push(&item.filename);

        if path.exists() {
            match archive_type(&item.filename)? {
                ArchiveType::DynamicIndex => {
                    let index = DynamicIndexReader::open(&path)?;
                    let (csum, size) = index.compute_csum();
                    match manifest.verify_file(&item.filename, &csum, size) {
                        Ok(_) => continue,
                        Err(err) => {
                            worker.log(format!("detected changed file {:?} - {}", path, err));
                        }
                    }
                }
                ArchiveType::FixedIndex => {
                    let index = FixedIndexReader::open(&path)?;
                    let (csum, size) = index.compute_csum();
                    match manifest.verify_file(&item.filename, &csum, size) {
                        Ok(_) => continue,
                        Err(err) => {
                            worker.log(format!("detected changed file {:?} - {}", path, err));
                        }
                    }
                }
                ArchiveType::Blob => {
                    let mut tmpfile = std::fs::File::open(&path)?;
                    let (csum, size) = compute_file_csum(&mut tmpfile)?;
                    match manifest.verify_file(&item.filename, &csum, size) {
                        Ok(_) => continue,
                        Err(err) => {
                            worker.log(format!("detected changed file {:?} - {}", path, err));
                        }
                    }
                }
            }
        }

        pull_single_archive(
            worker,
            &reader,
            &mut chunk_reader,
            tgt_store.clone(),
            snapshot,
            &item.filename,
        ).await?;
    }

    if let Err(err) = std::fs::rename(&tmp_manifest_name, &manifest_name) {
        bail!("Atomic rename file {:?} failed - {}", manifest_name, err);
    }

    if !client_log_name.exists() {
        try_client_log_download(worker, reader, &client_log_name).await?;
    }

    // cleanup - remove stale files
    tgt_store.cleanup_backup_dir(snapshot, &manifest)?;

    Ok(())
}

pub async fn pull_snapshot_from(
    worker: &WorkerTask,
    reader: Arc<BackupReader>,
    tgt_store: Arc<DataStore>,
    snapshot: &BackupDir,
) -> Result<(), Error> {

    let (_path, is_new) = tgt_store.create_backup_dir(&snapshot)?;

    if is_new {
        worker.log(format!("sync snapshot {:?}", snapshot.relative_path()));

        if let Err(err) = pull_snapshot(worker, reader, tgt_store.clone(), &snapshot).await {
            if let Err(cleanup_err) = tgt_store.remove_backup_dir(&snapshot) {
                worker.log(format!("cleanup error - {}", cleanup_err));
            }
            return Err(err);
        }
        worker.log(format!("sync snapshot {:?} done", snapshot.relative_path()));
    } else {
        worker.log(format!("re-sync snapshot {:?}", snapshot.relative_path()));
        pull_snapshot(worker, reader, tgt_store.clone(), &snapshot).await?;
        worker.log(format!("re-sync snapshot {:?} done", snapshot.relative_path()));
    }

    Ok(())
}

pub async fn pull_group(
    worker: &WorkerTask,
    client: &HttpClient,
    src_repo: &BackupRepository,
    tgt_store: Arc<DataStore>,
    group: &BackupGroup,
    delete: bool,
) -> Result<(), Error> {

    let path = format!("api2/json/admin/datastore/{}/snapshots", src_repo.store());

    let args = json!({
        "backup-type": group.backup_type(),
        "backup-id": group.backup_id(),
    });

    let mut result = client.get(&path, Some(args)).await?;
    let mut list: Vec<SnapshotListItem> = serde_json::from_value(result["data"].take())?;

    list.sort_unstable_by(|a, b| a.backup_time.cmp(&b.backup_time));

    let auth_info = client.login().await?;
    let fingerprint = client.fingerprint();

    let last_sync = tgt_store.last_successful_backup(group)?;

    let mut remote_snapshots = std::collections::HashSet::new();

    for item in list {
        let backup_time = Utc.timestamp(item.backup_time, 0);
        remote_snapshots.insert(backup_time);

        if let Some(last_sync_time) = last_sync {
            if last_sync_time > backup_time { continue; }
        }

        let options = HttpClientOptions::new()
            .password(Some(auth_info.ticket.clone()))
            .fingerprint(fingerprint.clone());

        let new_client = HttpClient::new(src_repo.host(), src_repo.user(), options)?;

        let reader = BackupReader::start(
            new_client,
            None,
            src_repo.store(),
            &item.backup_type,
            &item.backup_id,
            backup_time,
            true,
        ).await?;

        let snapshot = BackupDir::new(item.backup_type, item.backup_id, item.backup_time);

        pull_snapshot_from(worker, reader, tgt_store.clone(), &snapshot).await?;
    }

    if delete {
        let local_list = group.list_backups(&tgt_store.base_path())?;
        for info in local_list {
            let backup_time = info.backup_dir.backup_time();
            if remote_snapshots.contains(&backup_time) { continue; }
            worker.log(format!("delete vanished snapshot {:?}", info.backup_dir.relative_path()));
            tgt_store.remove_backup_dir(&info.backup_dir)?;
        }
    }

    Ok(())
}

pub async fn pull_store(
    worker: &WorkerTask,
    client: &HttpClient,
    src_repo: &BackupRepository,
    tgt_store: Arc<DataStore>,
    delete: bool,
    username: String,
) -> Result<(), Error> {

    // explicit create shared lock to prevent GC on newly created chunks
    let _shared_store_lock = tgt_store.try_shared_chunk_store_lock()?;

    let path = format!("api2/json/admin/datastore/{}/groups", src_repo.store());

    let mut result = client.get(&path, None).await?;

    let mut list: Vec<GroupListItem> = serde_json::from_value(result["data"].take())?;

    list.sort_unstable_by(|a, b| {
        let type_order = a.backup_type.cmp(&b.backup_type);
        if type_order == std::cmp::Ordering::Equal {
            a.backup_id.cmp(&b.backup_id)
        } else {
            type_order
        }
    });

    let mut errors = false;

    let mut new_groups = std::collections::HashSet::new();
    for item in list.iter() {
        new_groups.insert(BackupGroup::new(&item.backup_type, &item.backup_id));
    }

    for item in list {
        let group = BackupGroup::new(&item.backup_type, &item.backup_id);

        let owner = tgt_store.create_backup_group(&group, &username)?;
        // permission check
        if owner != username { // only the owner is allowed to create additional snapshots
            worker.log(format!("sync group {}/{} failed - owner check failed ({} != {})",
                               item.backup_type, item.backup_id, username, owner));
            errors = true;
            continue; // do not stop here, instead continue
        }

        if let Err(err) = pull_group(worker, client, src_repo, tgt_store.clone(), &group, delete).await {
            worker.log(format!("sync group {}/{} failed - {}", item.backup_type, item.backup_id, err));
            errors = true;
            continue; // do not stop here, instead continue
        }
    }

    if delete {
        let result: Result<(), Error> = proxmox::try_block!({
            let local_groups = BackupGroup::list_groups(&tgt_store.base_path())?;
            for local_group in local_groups {
                if new_groups.contains(&local_group) { continue; }
                worker.log(format!("delete vanished group '{}/{}'", local_group.backup_type(), local_group.backup_id()));
                if let Err(err) = tgt_store.remove_backup_group(&local_group) {
                    worker.log(err.to_string());
                    errors = true;
                }
            }
            Ok(())
        });
        if let Err(err) = result {
            worker.log(format!("error during cleanup: {}", err));
            errors = true;
        };
    }

    if errors {
        bail!("sync failed with some errors.");
    }

    Ok(())
}
