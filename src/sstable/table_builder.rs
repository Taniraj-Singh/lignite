//! Assembles data, filter, metaindex and index blocks into a complete SSTable
//! image held in memory.
//!
//! Keys must be presented in ascending internal-key order (the flush and
//! compaction paths guarantee this). Each data block is flushed once it reaches
//! the configured block size; its maximum key and [`BlockHandle`] become an entry
//! in the index block. Filter generation is driven off the running file offset.

use std::sync::Arc;

use crate::filter::FilterPolicy;
use crate::key::{extract_user_key, InternalKeyComparator};
use crate::options::CompressionType;
use crate::sstable::block_builder::BlockBuilder;
use crate::sstable::filter_block::FilterBlockBuilder;
use crate::sstable::format::{BlockHandle, Footer, BLOCK_TRAILER_SIZE};
use crate::util::coding::encode_fixed32;
use crate::util::crc32c;

/// Options controlling a single table build.
pub struct TableBuildOptions {
    /// Target uncompressed size of a data block before it is flushed.
    pub block_size: usize,
    /// Restart interval for delta-encoded keys within a block.
    pub block_restart_interval: usize,
    /// Block compression (only [`CompressionType::None`] is emitted today).
    pub compression: CompressionType,
    /// Optional filter policy; when present a filter block is written.
    pub filter_policy: Option<Arc<dyn FilterPolicy>>,
    /// Internal-key comparator (used for index bookkeeping).
    pub icmp: InternalKeyComparator,
}

/// Builds one SSTable image.
pub struct TableBuilder {
    opts: TableBuildOptions,
    file: Vec<u8>,
    offset: u64,
    data_block: BlockBuilder,
    index_block: BlockBuilder,
    filter_block: Option<FilterBlockBuilder>,
    last_key: Vec<u8>,
    num_entries: u64,
    finished: bool,
}

impl TableBuilder {
    /// Start a new table.
    pub fn new(opts: TableBuildOptions) -> Self {
        let filter_block = opts
            .filter_policy
            .as_ref()
            .map(|p| FilterBlockBuilder::new(Arc::clone(p)));
        let restart = opts.block_restart_interval;
        let mut tb = TableBuilder {
            opts,
            file: Vec::new(),
            offset: 0,
            data_block: BlockBuilder::new(restart),
            index_block: BlockBuilder::new(restart),
            filter_block,
            last_key: Vec::new(),
            num_entries: 0,
            finished: false,
        };
        if let Some(fb) = &mut tb.filter_block {
            fb.start_block(0);
        }
        tb
    }

    /// Number of key/value pairs added so far.
    pub fn num_entries(&self) -> u64 {
        self.num_entries
    }

    /// Current encoded size of the table image.
    pub fn file_size(&self) -> u64 {
        self.offset
    }

    /// Add one internal key/value pair. Keys must be non-decreasing.
    pub fn add(&mut self, internal_key: &[u8], value: &[u8]) {
        debug_assert!(!self.finished);
        if let Some(fb) = &mut self.filter_block {
            fb.add_key(extract_user_key(internal_key));
        }
        self.data_block.add(internal_key, value);
        self.last_key.clear();
        self.last_key.extend_from_slice(internal_key);
        self.num_entries += 1;

        if self.data_block.current_size_estimate() >= self.opts.block_size {
            self.flush_data_block();
        }
    }

    fn flush_data_block(&mut self) {
        if self.data_block.is_empty() {
            return;
        }
        let contents = self.data_block.finish().to_vec();
        let handle = self.write_block(&contents, self.opts.compression);
        self.data_block.reset();

        // Index this block by its maximum key.
        let mut handle_encoding = Vec::new();
        handle.encode_to(&mut handle_encoding);
        let last_key = std::mem::take(&mut self.last_key);
        self.index_block.add(&last_key, &handle_encoding);
        self.last_key = last_key;

        if let Some(fb) = &mut self.filter_block {
            fb.start_block(self.offset);
        }
    }

    /// Compress `contents` under `compression` (falling back to verbatim storage
    /// when compression does not shrink it) and append the stored block.
    fn write_block(&mut self, contents: &[u8], compression: CompressionType) -> BlockHandle {
        let (kind, stored) = crate::compress::encode(compression, contents);
        self.write_raw_block(&stored, kind)
    }

    /// Append already-encoded `stored` bytes with the given compression byte and a
    /// masked CRC trailer, returning a handle covering just the stored bytes.
    ///
    /// The CRC is taken over the *stored* (possibly compressed) bytes plus the
    /// type byte, matching the verification performed by the reader.
    fn write_raw_block(&mut self, stored: &[u8], compression: CompressionType) -> BlockHandle {
        let handle = BlockHandle::new(self.offset, stored.len() as u64);
        self.file.extend_from_slice(stored);
        let type_byte = compression as u8;
        let crc = crc32c::extend(crc32c::value(stored), &[type_byte]);
        self.file.push(type_byte);
        encode_fixed32(&mut self.file, crc32c::mask(crc));
        self.offset += (stored.len() + BLOCK_TRAILER_SIZE) as u64;
        handle
    }

    /// Finish the table and return the complete image.
    pub fn finish(mut self) -> Vec<u8> {
        self.flush_data_block();
        self.finished = true;

        // Filter block (always stored uncompressed).
        let mut filter_handle = BlockHandle::default();
        let mut have_filter = false;
        if let Some(mut fb) = self.filter_block.take() {
            let contents = fb.finish();
            filter_handle = self.write_raw_block(&contents, CompressionType::None);
            have_filter = true;
        }

        // Metaindex block: "filter.<policy>" -> filter handle.
        let mut metaindex = BlockBuilder::new(self.opts.block_restart_interval);
        if have_filter {
            if let Some(policy) = &self.opts.filter_policy {
                let key = format!("filter.{}", policy.name());
                let mut value = Vec::new();
                filter_handle.encode_to(&mut value);
                metaindex.add(key.as_bytes(), &value);
            }
        }
        let metaindex_contents = metaindex.finish().to_vec();
        let metaindex_handle = self.write_raw_block(&metaindex_contents, CompressionType::None);

        // Index block.
        let index_contents = self.index_block.finish().to_vec();
        let index_handle = self.write_raw_block(&index_contents, CompressionType::None);

        // Footer.
        let footer = Footer {
            metaindex_handle,
            index_handle,
        };
        footer.encode_to(&mut self.file);
        self.file
    }
}
