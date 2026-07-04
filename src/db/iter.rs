//! The public database iterator.
//!
//! It wraps the internal [`MergingIterator`] over all live sources and collapses
//! the MVCC version chains it yields into a stream of live user key/value pairs
//! visible at a fixed sequence number. For each user key it consumes versions in
//! the order the merge presents them, adopts the first one at or below the read's
//! sequence number, and — if that version is a tombstone — omits the key
//! entirely. Older versions of a key already decided are skipped.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::comparator::Comparator;
use crate::error::Result;
use crate::iterator::merging::MergingIterator;
use crate::iterator::{InternalIterator, LigniteIterator};
use crate::key::{ParsedInternalKey, SequenceNumber, ValueType};

/// A forward cursor over live user keys at a snapshot.
pub struct DbIterator {
    merge: MergingIterator,
    user_cmp: Arc<dyn Comparator>,
    seq: SequenceNumber,
    valid: bool,
    cur_key: Vec<u8>,
    cur_value: Vec<u8>,
    last_user_key: Vec<u8>,
    has_last: bool,
    error: Option<String>,
}

impl DbIterator {
    pub(crate) fn new(
        merge: MergingIterator,
        user_cmp: Arc<dyn Comparator>,
        seq: SequenceNumber,
    ) -> Self {
        DbIterator {
            merge,
            user_cmp,
            seq,
            valid: false,
            cur_key: Vec::new(),
            cur_value: Vec::new(),
            last_user_key: Vec::new(),
            has_last: false,
            error: None,
        }
    }

    /// Advance to the next visible user entry, skipping invisible versions,
    /// superseded versions and deleted keys.
    fn advance(&mut self) {
        loop {
            if !self.merge.valid() {
                self.valid = false;
                return;
            }
            let user_owned = match ParsedInternalKey::decode(self.merge.key()) {
                Some(p) => p.user_key.to_vec(),
                None => {
                    self.fail("iterator saw malformed internal key");
                    return;
                }
            };

            if self.has_last
                && self.user_cmp.compare(&user_owned, &self.last_user_key) == Ordering::Equal
            {
                // An older version of a key we have already resolved.
                self.merge.next();
                continue;
            }

            let mut decided = false;
            let mut value: Option<Vec<u8>> = None;
            loop {
                if !self.merge.valid() {
                    break;
                }
                let (pseq, ptype, same) = match ParsedInternalKey::decode(self.merge.key()) {
                    Some(p) => (
                        p.sequence,
                        p.value_type,
                        self.user_cmp.compare(p.user_key, &user_owned) == Ordering::Equal,
                    ),
                    None => {
                        self.fail("iterator saw malformed internal key");
                        return;
                    }
                };
                if !same {
                    break; // reached the next user key
                }
                if pseq <= self.seq {
                    match ptype {
                        ValueType::Value => value = Some(self.merge.value().to_vec()),
                        ValueType::Deletion => value = None,
                    }
                    decided = true;
                    self.merge.next();
                    break;
                } else {
                    // Version newer than the snapshot: not visible.
                    self.merge.next();
                }
            }

            self.last_user_key = user_owned.clone();
            self.has_last = true;

            if decided {
                if let Some(v) = value {
                    self.cur_key = user_owned;
                    self.cur_value = v;
                    self.valid = true;
                    return;
                }
                // Deleted at this snapshot: skip to the next user key.
                continue;
            }
            // No visible version for this user key.
        }
    }

    fn fail(&mut self, msg: &str) {
        self.error = Some(msg.to_string());
        self.valid = false;
    }

    /// Collect the remaining `(key, value)` pairs into a vector (convenience).
    pub fn collect_all(mut self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        self.seek_to_first();
        while self.valid() {
            out.push((self.key().to_vec(), self.value().to_vec()));
            self.next();
        }
        out
    }
}

impl LigniteIterator for DbIterator {
    fn valid(&self) -> bool {
        self.valid
    }

    fn seek_to_first(&mut self) {
        self.merge.seek_to_first();
        self.has_last = false;
        self.advance();
    }

    fn seek(&mut self, target: &[u8]) {
        let key = crate::db::Db::scan_seek_key(target);
        self.merge.seek(&key);
        self.has_last = false;
        self.advance();
    }

    fn next(&mut self) {
        if !self.valid {
            return;
        }
        self.advance();
    }

    fn key(&self) -> &[u8] {
        &self.cur_key
    }

    fn value(&self) -> &[u8] {
        &self.cur_value
    }

    fn status(&self) -> Result<()> {
        if let Some(msg) = &self.error {
            return Err(crate::error::Error::corruption(msg.clone()));
        }
        self.merge.status()
    }
}
