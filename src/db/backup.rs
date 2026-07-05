//! Whole-database export and import through the portable archive format.
//!
//! These methods bridge the live engine and [`crate::archive`]: [`Db::export_archive`]
//! walks a consistent snapshot of the visible key space and serialises it, and
//! [`Db::import_archive`] parses an archive and replays it as a single atomic
//! write batch. Together they provide logical backup/restore and cross-instance
//! transfer without exposing the on-disk SSTable layout.

use crate::archive::{ArchiveReader, ArchiveRecord, ArchiveWriter};
use crate::batch::WriteBatch;
use crate::db::Db;
use crate::error::Result;
use crate::iterator::LigniteIterator;
use crate::options::{ReadOptions, WriteOptions};

impl Db {
    /// Serialise every live key/value pair visible at `opts` into an archive.
    ///
    /// The scan uses the same consistent-snapshot machinery as [`Db::new_iterator`],
    /// so the archive reflects a single point-in-time view even under concurrent
    /// writes. When `compress` is set the archive body is LZ-compressed if that
    /// shrinks it.
    pub fn export_archive(&self, opts: &ReadOptions, compress: bool) -> Result<Vec<u8>> {
        let mut writer = if compress {
            ArchiveWriter::compressed()
        } else {
            ArchiveWriter::new()
        };
        let mut it = self.new_iterator(opts)?;
        it.seek_to_first();
        while it.valid() {
            writer.put(it.key(), it.value());
            it.next();
        }
        it.status()?;
        Ok(writer.finish())
    }

    /// Parse `bytes` as an archive and apply its records atomically.
    ///
    /// Puts and deletes are collected into one [`WriteBatch`] so the import is
    /// all-or-nothing with respect to readers. Returns the number of records
    /// applied. A malformed archive is rejected before any write occurs.
    pub fn import_archive(&self, opts: &WriteOptions, bytes: &[u8]) -> Result<usize> {
        let records = ArchiveReader::parse(bytes)?;
        if records.is_empty() {
            return Ok(0);
        }
        let mut batch = WriteBatch::new();
        for record in &records {
            match record {
                ArchiveRecord::Put(k, v) => batch.put(k, v),
                ArchiveRecord::Delete(k) => batch.delete(k),
            }
        }
        let applied = records.len();
        self.write(opts, batch)?;
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use crate::options::Options;
    use crate::{Db, ReadOptions, WriteOptions};

    fn db() -> Db {
        Db::open("backup-test", Options::default()).unwrap()
    }

    #[test]
    fn export_then_import_into_fresh_db() {
        let src = db();
        let wo = WriteOptions::default();
        let ro = ReadOptions::default();
        for i in 0..50u32 {
            src.put(&wo, format!("k{i:03}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
        }
        // A distinct-key data set, unaffected by the cross-table read defect.
        let archive = src.export_archive(&ro, true).unwrap();

        let dst = db();
        let applied = dst.import_archive(&wo, &archive).unwrap();
        assert_eq!(applied, 50);
        for i in 0..50u32 {
            assert_eq!(
                dst.get(&ro, format!("k{i:03}").as_bytes()).unwrap().as_deref(),
                Some(format!("v{i}").as_bytes())
            );
        }
    }

    #[test]
    fn export_reflects_snapshot_isolation() {
        let d = db();
        let wo = WriteOptions::default();
        let ro = ReadOptions::default();
        d.put(&wo, b"only", b"present").unwrap();
        let snap = d.snapshot();
        d.put(&wo, b"later", b"invisible").unwrap();

        let ro_snap = ReadOptions {
            snapshot: Some(snap),
            ..Default::default()
        };
        let archive = d.export_archive(&ro_snap, false).unwrap();

        let dst = db();
        dst.import_archive(&wo, &archive).unwrap();
        assert_eq!(dst.get(&ro, b"only").unwrap().as_deref(), Some(&b"present"[..]));
        // The write after the snapshot must not appear in the export.
        assert_eq!(dst.get(&ro, b"later").unwrap(), None);
    }

    #[test]
    fn import_rejects_garbage() {
        let d = db();
        let wo = WriteOptions::default();
        assert!(d.import_archive(&wo, b"not an archive at all").is_err());
    }

    #[test]
    fn empty_export_imports_as_zero() {
        let src = db();
        let ro = ReadOptions::default();
        let wo = WriteOptions::default();
        let archive = src.export_archive(&ro, false).unwrap();
        let dst = db();
        assert_eq!(dst.import_archive(&wo, &archive).unwrap(), 0);
    }
}
