use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use failure::*;
use lazy_static::lazy_static;

use super::backup_info::BackupDir;
use super::chunk_store::{ChunkStore, GarbageCollectionStatus};
use super::dynamic_index::{DynamicIndexReader, DynamicIndexWriter};
use super::fixed_index::{FixedIndexReader, FixedIndexWriter};
use super::index::*;
use super::{DataBlob, ArchiveType, archive_type};
use crate::config::datastore;
use crate::server::WorkerTask;
use crate::tools;

lazy_static! {
    static ref DATASTORE_MAP: Mutex<HashMap<String, Arc<DataStore>>> = Mutex::new(HashMap::new());
}

/// Datastore Management
///
/// A Datastore can store severals backups, and provides the
/// management interface for backup.
pub struct DataStore {
    chunk_store: Arc<ChunkStore>,
    gc_mutex: Mutex<bool>,
    last_gc_status: Mutex<GarbageCollectionStatus>,
}

impl DataStore {

    pub fn lookup_datastore(name: &str) -> Result<Arc<DataStore>, Error> {

        let config = datastore::config()?;
        let (_, store_config) = config.sections.get(name)
            .ok_or(format_err!("no such datastore '{}'", name))?;

        let path = store_config["path"].as_str().unwrap();

        let mut map = DATASTORE_MAP.lock().unwrap();

        if let Some(datastore) = map.get(name) {
            // Compare Config - if changed, create new Datastore object!
            if datastore.chunk_store.base == PathBuf::from(path) {
                return Ok(datastore.clone());
            }
        }

        let datastore = DataStore::open(name)?;

        let datastore = Arc::new(datastore);
        map.insert(name.to_string(), datastore.clone());

        Ok(datastore)
    }

    pub fn open(store_name: &str) -> Result<Self, Error> {

        let config = datastore::config()?;
        let (_, store_config) = config.sections.get(store_name)
            .ok_or(format_err!("no such datastore '{}'", store_name))?;

        let path = store_config["path"].as_str().unwrap();

        let chunk_store = ChunkStore::open(store_name, path)?;

        let gc_status = GarbageCollectionStatus::default();

        Ok(Self {
            chunk_store: Arc::new(chunk_store),
            gc_mutex: Mutex::new(false),
            last_gc_status: Mutex::new(gc_status),
        })
    }

    pub fn get_chunk_iterator(
        &self,
    ) -> Result<
        impl Iterator<Item = (Result<tools::fs::ReadDirEntry, Error>, usize)>,
        Error
    > {
        self.chunk_store.get_chunk_iterator()
    }

    pub fn create_fixed_writer<P: AsRef<Path>>(&self, filename: P, size: usize, chunk_size: usize) -> Result<FixedIndexWriter, Error> {

        let index = FixedIndexWriter::create(self.chunk_store.clone(), filename.as_ref(), size, chunk_size)?;

        Ok(index)
    }

    pub fn open_fixed_reader<P: AsRef<Path>>(&self, filename: P) -> Result<FixedIndexReader, Error> {

        let full_path =  self.chunk_store.relative_path(filename.as_ref());

        let index = FixedIndexReader::open(&full_path)?;

        Ok(index)
    }

    pub fn create_dynamic_writer<P: AsRef<Path>>(
        &self, filename: P,
    ) -> Result<DynamicIndexWriter, Error> {

        let index = DynamicIndexWriter::create(
            self.chunk_store.clone(), filename.as_ref())?;

        Ok(index)
    }

    pub fn open_dynamic_reader<P: AsRef<Path>>(&self, filename: P) -> Result<DynamicIndexReader, Error> {

        let full_path =  self.chunk_store.relative_path(filename.as_ref());

        let index = DynamicIndexReader::open(&full_path)?;

        Ok(index)
    }

    pub fn open_index<P>(&self, filename: P) -> Result<Box<dyn IndexFile + Send>, Error>
    where
        P: AsRef<Path>,
    {
        let filename = filename.as_ref();
        let out: Box<dyn IndexFile + Send> =
            match archive_type(filename)? {
                ArchiveType::DynamicIndex => Box::new(self.open_dynamic_reader(filename)?),
                ArchiveType::FixedIndex => Box::new(self.open_fixed_reader(filename)?),
                _ => bail!("cannot open index file of unknown type: {:?}", filename),
            };
        Ok(out)
    }

    pub fn base_path(&self) -> PathBuf {
        self.chunk_store.base_path()
    }

    /// Remove a backup directory including all content
    pub fn remove_backup_dir(&self, backup_dir: &BackupDir,
    ) ->  Result<(), io::Error> {

        let relative_path = backup_dir.relative_path();
        let mut full_path = self.base_path();
        full_path.push(&relative_path);

        log::info!("removing backup {:?}", full_path);
        std::fs::remove_dir_all(full_path)?;

        Ok(())
    }

    pub fn create_backup_dir(&self, backup_dir: &BackupDir) ->  Result<(PathBuf, bool), io::Error> {

        // create intermediate path first:
        let mut full_path = self.base_path();
        full_path.push(backup_dir.group().group_path());
        std::fs::create_dir_all(&full_path)?;

        let relative_path = backup_dir.relative_path();
        let mut full_path = self.base_path();
        full_path.push(&relative_path);

        // create the last component now
        match std::fs::create_dir(&full_path) {
            Ok(_) => Ok((relative_path, true)),
            Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists => Ok((relative_path, false)),
            Err(e) => Err(e)
        }
    }

    pub fn list_images(&self) -> Result<Vec<PathBuf>, Error> {
        let base = self.base_path();

        let mut list = vec![];

        use walkdir::WalkDir;

        let walker = WalkDir::new(&base).same_file_system(true).into_iter();

        // make sure we skip .chunks (and other hidden files to keep it simple)
        fn is_hidden(entry: &walkdir::DirEntry) -> bool {
            entry.file_name()
                .to_str()
                .map(|s| s.starts_with("."))
                .unwrap_or(false)
        }

        for entry in walker.filter_entry(|e| !is_hidden(e)) {
            let path = entry?.into_path();
            if let Ok(archive_type) = archive_type(&path) {
                if archive_type == ArchiveType::FixedIndex || archive_type == ArchiveType::DynamicIndex {
                    list.push(path);
                }
            }
        }

        Ok(list)
    }

    // mark chunks  used by ``index`` as used
    fn index_mark_used_chunks<I: IndexFile>(
        &self,
        index: I,
        file_name: &Path, // only used for error reporting
        status: &mut GarbageCollectionStatus,
    ) -> Result<(), Error> {

        status.index_file_count += 1;
        status.index_data_bytes += index.index_bytes();

        for pos in 0..index.index_count() {
            tools::fail_on_shutdown()?;
            let digest = index.index_digest(pos).unwrap();
            if let Err(err) = self.chunk_store.touch_chunk(digest) {
                bail!("unable to access chunk {}, required by {:?} - {}",
                      proxmox::tools::digest_to_hex(digest), file_name, err);
            }
        }
        Ok(())
    }

    fn mark_used_chunks(&self, status: &mut GarbageCollectionStatus) -> Result<(), Error> {

        let image_list = self.list_images()?;

        for path in image_list {

            tools::fail_on_shutdown()?;

            if let Ok(archive_type) = archive_type(&path) {
                if archive_type == ArchiveType::FixedIndex {
                    let index = self.open_fixed_reader(&path)?;
                    self.index_mark_used_chunks(index, &path, status)?;
                } else if archive_type == ArchiveType::DynamicIndex {
                    let index = self.open_dynamic_reader(&path)?;
                    self.index_mark_used_chunks(index, &path, status)?;
                }
            }
        }

        Ok(())
    }

    pub fn last_gc_status(&self) -> GarbageCollectionStatus {
        self.last_gc_status.lock().unwrap().clone()
    }

    pub fn garbage_collection(&self, worker: Arc<WorkerTask>) -> Result<(), Error> {

        if let Ok(ref mut _mutex) = self.gc_mutex.try_lock() {

            let _exclusive_lock =  self.chunk_store.try_exclusive_lock()?;

            let oldest_writer = self.chunk_store.oldest_writer();

            let mut gc_status = GarbageCollectionStatus::default();
            gc_status.upid = Some(worker.to_string());

            worker.log("Start GC phase1 (mark used chunks)");

            self.mark_used_chunks(&mut gc_status)?;

            worker.log("Start GC phase2 (sweep unused chunks)");
            self.chunk_store.sweep_unused_chunks(oldest_writer, &mut gc_status, worker.clone())?;

            worker.log(&format!("Removed bytes: {}", gc_status.removed_bytes));
            worker.log(&format!("Removed chunks: {}", gc_status.removed_chunks));
            worker.log(&format!("Original data bytes: {}", gc_status.index_data_bytes));

            if gc_status.index_data_bytes > 0 {
                let comp_per = (gc_status.disk_bytes*100)/gc_status.index_data_bytes;
                worker.log(&format!("Disk bytes: {} ({} %)", gc_status.disk_bytes, comp_per));
            }

            worker.log(&format!("Disk chunks: {}", gc_status.disk_chunks));

            if gc_status.disk_chunks > 0 {
                let avg_chunk = gc_status.disk_bytes/(gc_status.disk_chunks as u64);
                worker.log(&format!("Average chunk size: {}", avg_chunk));
            }

            *self.last_gc_status.lock().unwrap() = gc_status;

        } else {
            bail!("Start GC failed - (already running/locked)");
        }

        Ok(())
    }

    pub fn try_shared_chunk_store_lock(&self) -> Result<tools::ProcessLockSharedGuard, Error> {
        self.chunk_store.try_shared_lock()
    }

    pub fn chunk_path(&self, digest:&[u8; 32]) -> (PathBuf, String) {
        self.chunk_store.chunk_path(digest)
    }

    pub fn insert_chunk(
        &self,
        chunk: &DataBlob,
        digest: &[u8; 32],
    ) -> Result<(bool, u64), Error> {
        self.chunk_store.insert_chunk(chunk, digest)
    }
}
