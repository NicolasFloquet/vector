mod key;
mod reader;
mod writer;

use super::{DataDirError, Open};
use crate::buffer_usage_data::BufferUsageData;
use crate::bytes::{DecodeBytes, EncodeBytes};
use crate::Acker;
use futures::task::AtomicWaker;
use key::Key;
use leveldb::database::{
    batch::Writebatch,
    iterator::{Iterable, LevelDBIterator},
    options::{Options, ReadOptions},
    Database,
};
pub use reader::Reader;
use snafu::ResultExt;
use std::fmt::Debug;
use std::{
    collections::VecDeque,
    marker::PhantomData,
    path::Path,
    sync::{atomic::AtomicUsize, Arc, Mutex},
    time::Instant,
};
pub use writer::Writer;

/// How much of disk buffer needs to be deleted before we trigger compaction.
const MAX_UNCOMPACTED_DENOMINATOR: usize = 10;

#[derive(Default)]
pub struct Buffer<T> {
    phantom: PhantomData<T>,
}

/// Read the byte size of the database
///
/// There is a mismatch between leveldb's mechanism and vector's. While vector
/// would prefer to keep as little in-memory as possible leveldb, being a
/// database, has the opposite consideration. As such it may mmap 1000 of its
/// LDB files into vector's address space at a time with no ability for us to
/// change this number. See [leveldb issue
/// 866](https://github.com/google/leveldb/issues/866). Because we do need to
/// know the byte size of our store we are forced to iterate through all the LDB
/// files on disk, meaning we impose a huge memory burden on our end users right
/// at the jump in conditions where the disk buffer has filled up. This'll OOM
/// vector, meaning we're trapped in a catch 22.
///
/// This function does not solve the problem -- leveldb will still map 1000
/// files if it wants -- but we at least avoid forcing this to happen at the
/// start of vector.
fn db_initial_size(path: &Path) -> Result<usize, DataDirError> {
    let mut options = Options::new();
    options.create_if_missing = true;
    let db: Database<Key> = Database::open(path, options).with_context(|| Open {
        data_dir: path.parent().expect("always a parent"),
    })?;
    Ok(db.value_iter(ReadOptions::new()).map(|v| v.len()).sum())
}

impl<T> Buffer<T>
where
    T: Send + Sync + Unpin + EncodeBytes<T> + DecodeBytes<T>,
    <T as EncodeBytes<T>>::Error: Debug,
    <T as DecodeBytes<T>>::Error: Debug,
{
    /// Build a new `DiskBuffer` rooted at `path`
    ///
    /// # Errors
    ///
    /// Function will fail if the permissions of `path` are not correct, if
    /// there is no space available on disk etc.
    #[allow(clippy::cast_precision_loss)]
    pub fn build(
        path: &Path,
        max_size: usize,
        buffer_usage_data: Arc<BufferUsageData>,
    ) -> Result<(Writer<T>, Reader<T>, Acker), DataDirError> {
        // New `max_size` of the buffer is used for storing the unacked events.
        // The rest is used as a buffer which when filled triggers compaction.
        let max_uncompacted_size = max_size / MAX_UNCOMPACTED_DENOMINATOR;
        let max_size = max_size - max_uncompacted_size;

        let initial_size = db_initial_size(path)?;

        let mut options = Options::new();
        options.create_if_missing = true;

        let db: Database<Key> = Database::open(path, options).with_context(|| Open {
            data_dir: path.parent().expect("always a parent"),
        })?;
        let db = Arc::new(db);

        let head;
        let tail;
        {
            let mut iter = db.keys_iter(ReadOptions::new());
            head = iter.next().map_or(0, |k| k.0);
            iter.seek_to_last();
            tail = if iter.valid() { iter.key().0 + 1 } else { 0 };
        }

        let current_size = Arc::new(AtomicUsize::new(initial_size));

        let write_notifier = Arc::new(AtomicWaker::new());

        let blocked_write_tasks = Arc::new(Mutex::new(Vec::new()));

        let ack_counter = Arc::new(AtomicUsize::new(0));
        let acker = Acker::Disk(Arc::clone(&ack_counter), Arc::clone(&write_notifier));

        let writer = Writer {
            db: Some(Arc::clone(&db)),
            write_notifier: Arc::clone(&write_notifier),
            blocked_write_tasks: Arc::clone(&blocked_write_tasks),
            offset: Arc::new(AtomicUsize::new(tail)),
            writebatch: Writebatch::new(),
            batch_size: 0,
            max_size,
            current_size: Arc::clone(&current_size),
            slot: None,
            buffer_usage_data: buffer_usage_data.clone(),
        };

        let mut reader = Reader {
            db: Arc::clone(&db),
            write_notifier: Arc::clone(&write_notifier),
            blocked_write_tasks,
            read_offset: head,
            compacted_offset: 0,
            acked: 0,
            delete_offset: head,
            current_size,
            ack_counter,
            max_uncompacted_size,
            uncompacted_size: 0,
            unacked_sizes: VecDeque::new(),
            buffer: VecDeque::new(),
            last_compaction: Instant::now(),
            pending_read: None,
            phantom: PhantomData,
            buffer_usage_data,
        };
        // Compact on every start
        reader.compact();

        Ok((writer, reader, acker))
    }
}
