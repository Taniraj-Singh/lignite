//! Reads an SSTable image and exposes it as an [`InternalIterator`] via a
//! two-level index → data-block cursor.
//!
//! On open the footer is parsed, the index and metaindex blocks are loaded, and
//! (if present) the filter block is wrapped in a [`FilterBlockReader`]. Iteration
//! walks the index block; each index value is a [`BlockHandle`] naming a data
//! block, which is read and iterated in turn.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::filter::FilterPolicy;
use crate::iterator::InternalIterator;
use crate::key::InternalKeyComparator;
use crate::sstable::block::{Block, BlockIter};
use crate::sstable::filter_block::FilterBlockReader;
use crate::sstable::format::{
    parse_compression, BlockHandle, Footer, BLOCK_TRAILER_SIZE, FOOTER_ENCODED_LENGTH,
};
use crate::options::CompressionType;
use crate::util::coding::decode_fixed32;
use crate::util::crc32c;

/// A read-only handle to one SSTable.
pub struct Table {
    file: Arc<Vec<u8>>,
    index_block: Block,
    icmp: InternalKeyComparator,
    filter: Option<FilterBlockReader>,
    verify_checksums: bool,
}

impl Table {
    /// Parse the table image `file`, ordered by `icmp`. `filter_policy` must
    /// match the policy the table was written with for filters to be usable.
    pub fn open(
        file: Arc<Vec<u8>>,
        icmp: InternalKeyComparator,
        filter_policy: Option<Arc<dyn FilterPolicy>>,
        verify_checksums: bool,
    ) -> Result<Table> {
        if file.len() < FOOTER_ENCODED_LENGTH {
            return Err(Error::corruption("file shorter than footer"));
        }
        let footer_start = file.len() - FOOTER_ENCODED_LENGTH;
        let footer = Footer::decode_from(&file[footer_start..])?;

        let index_bytes = read_block(&file, footer.index_handle, verify_checksums)?;
        let index_block = Block::new(index_bytes)?;

        let filter = Self::read_filter(&file, &footer, filter_policy, verify_checksums)?;

        Ok(Table {
            file,
            index_block,
            icmp,
            filter,
            verify_checksums,
        })
    }

    fn read_filter(
        file: &Arc<Vec<u8>>,
        footer: &Footer,
        filter_policy: Option<Arc<dyn FilterPolicy>>,
        verify: bool,
    ) -> Result<Option<FilterBlockReader>> {
        let policy = match filter_policy {
            Some(p) => p,
            None => return Ok(None),
        };
        let metaindex_bytes = read_block(file, footer.metaindex_handle, verify)?;
        let metaindex = Block::new(metaindex_bytes)?;
        let icmp = InternalKeyComparator::new(Arc::new(crate::comparator::BytewiseComparator));
        // Metaindex keys are raw names, not internal keys, but a bytewise scan
        // suffices to find the filter entry.
        let mut it = metaindex.iter(icmp);
        let wanted = format!("filter.{}", policy.name());
        it.seek_to_first();
        while it.valid() {
            if it.key() == wanted.as_bytes() {
                let (handle, _) = BlockHandle::decode_from(it.value())?;
                let contents = read_block(file, handle, verify)?;
                return Ok(Some(FilterBlockReader::new(policy, contents)));
            }
            it.next();
        }
        Ok(None)
    }

    /// Whether this table carries a usable filter.
    pub fn has_filter(&self) -> bool {
        self.filter.is_some()
    }

    /// Consult the filter (if any): could `user_key` live in the data block at
    /// `block_offset`? Absent a filter, always `true`.
    pub fn filter_may_match(&self, block_offset: u64, user_key: &[u8]) -> bool {
        match &self.filter {
            Some(f) => f.key_may_match(block_offset, user_key),
            None => true,
        }
    }

    /// A cursor over the whole table in internal-key order.
    pub fn iter(&self) -> TableIter {
        TableIter {
            file: Arc::clone(&self.file),
            icmp: self.icmp.clone(),
            verify: self.verify_checksums,
            index_iter: self.index_block.iter(self.icmp.clone()),
            data_iter: None,
            data_block_handle: Vec::new(),
            error: None,
        }
    }
}

/// Read a block's contents by handle, verifying its CRC when requested.
pub fn read_block(
    file: &[u8],
    handle: BlockHandle,
    verify_checksums: bool,
) -> Result<Arc<Vec<u8>>> {
    let offset = handle.offset as usize;
    let size = handle.size as usize;
    let end = offset
        .checked_add(size + BLOCK_TRAILER_SIZE)
        .ok_or_else(|| Error::corruption("block handle overflows file"))?;
    if end > file.len() {
        return Err(Error::corruption("block handle out of range"));
    }
    let contents = &file[offset..offset + size];
    let type_byte = file[offset + size];
    if verify_checksums {
        let stored = decode_fixed32(&file[offset + size + 1..end])
            .ok_or_else(|| Error::corruption("missing block crc"))?;
        let computed = crc32c::mask(crc32c::extend(crc32c::value(contents), &[type_byte]));
        if stored != computed {
            return Err(Error::corruption("block checksum mismatch"));
        }
    }
    match parse_compression(type_byte)? {
        CompressionType::None => Ok(Arc::new(contents.to_vec())),
        kind => Ok(Arc::new(crate::compress::decode(kind, contents)?)),
    }
}

/// A two-level (index → data) cursor over a [`Table`].
pub struct TableIter {
    file: Arc<Vec<u8>>,
    icmp: InternalKeyComparator,
    verify: bool,
    index_iter: BlockIter,
    data_iter: Option<BlockIter>,
    data_block_handle: Vec<u8>,
    error: Option<String>,
}

impl TableIter {
    fn set_data_iterator(&mut self, iter: Option<BlockIter>) {
        self.data_iter = iter;
    }

    fn init_data_block(&mut self) {
        if !self.index_iter.valid() {
            self.set_data_iterator(None);
            return;
        }
        let handle_encoding = self.index_iter.value().to_vec();
        if self.data_iter.is_some() && handle_encoding == self.data_block_handle {
            return; // already positioned on this block
        }
        match BlockHandle::decode_from(&handle_encoding) {
            Ok((handle, _)) => match read_block(&self.file, handle, self.verify) {
                Ok(bytes) => match Block::new(bytes) {
                    Ok(block) => {
                        self.data_block_handle = handle_encoding;
                        self.set_data_iterator(Some(block.iter(self.icmp.clone())));
                    }
                    Err(e) => self.record_error(e),
                },
                Err(e) => self.record_error(e),
            },
            Err(e) => self.record_error(e),
        }
    }

    fn record_error(&mut self, err: Error) {
        self.error = Some(err.to_string());
        self.set_data_iterator(None);
    }

    fn skip_empty_data_blocks_forward(&mut self) {
        while self.data_iter.as_ref().map(|d| !d.valid()).unwrap_or(true) {
            if !self.index_iter.valid() {
                self.set_data_iterator(None);
                return;
            }
            self.index_iter.next();
            self.init_data_block();
            if let Some(d) = &mut self.data_iter {
                d.seek_to_first();
            }
        }
    }

    fn skip_empty_data_blocks_backward(&mut self) {
        while self.data_iter.as_ref().map(|d| !d.valid()).unwrap_or(true) {
            if !self.index_iter.valid() {
                self.set_data_iterator(None);
                return;
            }
            self.index_iter.prev();
            self.init_data_block();
            if let Some(d) = &mut self.data_iter {
                d.seek_to_last();
            }
        }
    }
}

impl InternalIterator for TableIter {
    fn valid(&self) -> bool {
        self.data_iter.as_ref().map(|d| d.valid()).unwrap_or(false)
    }

    fn seek_to_first(&mut self) {
        self.index_iter.seek_to_first();
        self.init_data_block();
        if let Some(d) = &mut self.data_iter {
            d.seek_to_first();
        }
        self.skip_empty_data_blocks_forward();
    }

    fn seek_to_last(&mut self) {
        self.index_iter.seek_to_last();
        self.init_data_block();
        if let Some(d) = &mut self.data_iter {
            d.seek_to_last();
        }
        self.skip_empty_data_blocks_backward();
    }

    fn seek(&mut self, target: &[u8]) {
        self.index_iter.seek(target);
        self.init_data_block();
        if let Some(d) = &mut self.data_iter {
            d.seek(target);
        }
        self.skip_empty_data_blocks_forward();
    }

    fn next(&mut self) {
        if let Some(d) = &mut self.data_iter {
            d.next();
        }
        self.skip_empty_data_blocks_forward();
    }

    fn prev(&mut self) {
        if let Some(d) = &mut self.data_iter {
            d.prev();
        }
        self.skip_empty_data_blocks_backward();
    }

    fn key(&self) -> &[u8] {
        self.data_iter.as_ref().expect("key() on invalid TableIter").key()
    }

    fn value(&self) -> &[u8] {
        self.data_iter
            .as_ref()
            .expect("value() on invalid TableIter")
            .value()
    }

    fn status(&self) -> Result<()> {
        if let Some(msg) = &self.error {
            return Err(Error::corruption(msg.clone()));
        }
        self.index_iter.status()?;
        if let Some(d) = &self.data_iter {
            d.status()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;
    use crate::filter::BloomFilterPolicy;
    use crate::key::{append_internal_key, extract_user_key, ValueType};
    use crate::sstable::table_builder::{TableBuildOptions, TableBuilder};

    fn icmp() -> InternalKeyComparator {
        InternalKeyComparator::new(Arc::new(BytewiseComparator))
    }

    fn build_table(pairs: &[(Vec<u8>, u64, Vec<u8>)], block_size: usize) -> Arc<Vec<u8>> {
        let policy: Arc<dyn FilterPolicy> = Arc::new(BloomFilterPolicy::new(10));
        let opts = TableBuildOptions {
            block_size,
            block_restart_interval: 4,
            compression: CompressionType::None,
            filter_policy: Some(policy),
            icmp: icmp(),
        };
        let mut tb = TableBuilder::new(opts);
        for (u, seq, v) in pairs {
            let mut ik = Vec::new();
            append_internal_key(&mut ik, u, *seq, ValueType::Value);
            tb.add(&ik, v);
        }
        Arc::new(tb.finish())
    }

    #[test]
    fn roundtrip_small_table() {
        let pairs: Vec<(Vec<u8>, u64, Vec<u8>)> = (0..200u32)
            .map(|i| (format!("key{i:05}").into_bytes(), 1u64, format!("val{i}").into_bytes()))
            .collect();
        let file = build_table(&pairs, 128); // small blocks => many data blocks
        let policy: Arc<dyn FilterPolicy> = Arc::new(BloomFilterPolicy::new(10));
        let table = Table::open(file, icmp(), Some(policy), true).unwrap();
        assert!(table.has_filter());

        let mut it = table.iter();
        it.seek_to_first();
        let mut count = 0;
        let mut last: Option<Vec<u8>> = None;
        while it.valid() {
            let uk = extract_user_key(it.key()).to_vec();
            if let Some(p) = &last {
                assert!(p < &uk);
            }
            last = Some(uk);
            count += 1;
            it.next();
        }
        assert_eq!(count, 200);
    }

    #[test]
    fn seek_across_blocks() {
        let pairs: Vec<(Vec<u8>, u64, Vec<u8>)> = (0..200u32)
            .map(|i| (format!("key{i:05}").into_bytes(), 1u64, format!("val{i}").into_bytes()))
            .collect();
        let file = build_table(&pairs, 128);
        let table = Table::open(file, icmp(), None, true).unwrap();
        let mut it = table.iter();
        let mut target = Vec::new();
        append_internal_key(&mut target, b"key00137", crate::key::MAX_SEQUENCE_NUMBER, ValueType::Value);
        it.seek(&target);
        assert!(it.valid());
        assert_eq!(extract_user_key(it.key()), b"key00137");
        assert_eq!(it.value(), b"val137");
    }

    #[test]
    fn reverse_full_scan() {
        let pairs: Vec<(Vec<u8>, u64, Vec<u8>)> = (0..50u32)
            .map(|i| (format!("k{i:03}").into_bytes(), 1u64, format!("v{i}").into_bytes()))
            .collect();
        let file = build_table(&pairs, 64);
        let table = Table::open(file, icmp(), None, false).unwrap();
        let mut it = table.iter();
        it.seek_to_last();
        let mut count = 0;
        while it.valid() {
            count += 1;
            it.prev();
        }
        assert_eq!(count, 50);
    }

    #[test]
    fn corrupt_checksum_detected() {
        let pairs = vec![(b"a".to_vec(), 1u64, b"1".to_vec())];
        let file = build_table(&pairs, 4096);
        let mut bytes = (*file).clone();
        bytes[0] ^= 0xff; // flip a byte in the first data block
        let table = Table::open(Arc::new(bytes), icmp(), None, true);
        // Opening reads the index/metaindex with verification; the flipped data
        // block surfaces when iterated.
        if let Ok(t) = table {
            let mut it = t.iter();
            it.seek_to_first();
            // Either the iterator is invalid or reports an error.
            assert!(it.status().is_err() || !it.valid());
        }
    }
}
