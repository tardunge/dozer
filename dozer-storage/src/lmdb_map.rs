use std::{borrow::Cow, ops::Bound};

use lmdb::{Database, DatabaseFlags, RoCursor, RwTransaction, Transaction, WriteFlags};

use crate::{
    errors::StorageError,
    lmdb_storage::{LmdbEnvironmentManager, LmdbExclusiveTransaction},
    Iterator, KeyIterator, LmdbKey, LmdbValType, LmdbValue, ValueIterator,
};

#[derive(Debug)]
pub struct LmdbMap<K: ?Sized, V: ?Sized> {
    db: Database,
    _key: std::marker::PhantomData<*const K>,
    _value: std::marker::PhantomData<*const V>,
}

impl<K: ?Sized, V: ?Sized> Clone for LmdbMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            db: self.db,
            _key: std::marker::PhantomData,
            _value: std::marker::PhantomData,
        }
    }
}

impl<K: ?Sized, V: ?Sized> Copy for LmdbMap<K, V> {}

// Safety: `Database` is `Send` and `Sync`.
unsafe impl<K: ?Sized, V: ?Sized> Send for LmdbMap<K, V> {}
unsafe impl<K: ?Sized, V: ?Sized> Sync for LmdbMap<K, V> {}

impl<K: LmdbKey + ?Sized, V: LmdbValue + ?Sized> LmdbMap<K, V> {
    pub fn new_from_env(
        env: &mut LmdbEnvironmentManager,
        name: Option<&str>,
        create_if_not_exist: bool,
    ) -> Result<Self, StorageError> {
        let create_flags = if create_if_not_exist {
            Some(database_key_flag::<K>())
        } else {
            None
        };

        let db = env.create_database(name, create_flags)?;

        Ok(Self {
            db,
            _key: std::marker::PhantomData,
            _value: std::marker::PhantomData,
        })
    }

    pub fn new_from_txn(
        txn: &mut LmdbExclusiveTransaction,
        name: Option<&str>,
        create_if_not_exist: bool,
    ) -> Result<Self, StorageError> {
        let create_flags = if create_if_not_exist {
            Some(database_key_flag::<K>())
        } else {
            None
        };

        let db = txn.create_database(name, create_flags)?;

        Ok(Self {
            db,
            _key: std::marker::PhantomData,
            _value: std::marker::PhantomData,
        })
    }

    pub fn database(&self) -> Database {
        self.db
    }

    pub fn count<T: Transaction>(&self, txn: &T) -> Result<usize, StorageError> {
        Ok(lmdb_stat(txn, self.db).map(|stat| stat.ms_entries)?)
    }

    pub fn get<'a, T: Transaction>(
        &self,
        txn: &'a T,
        key: &K,
    ) -> Result<Option<Cow<'a, V>>, StorageError> {
        let key = key.encode()?;
        match txn.get(self.db, &key) {
            Ok(value) => Ok(Some(V::decode(value)?)),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Returns if the key was actually inserted.
    pub fn insert(
        &self,
        txn: &mut RwTransaction,
        key: &K,
        value: &V,
    ) -> Result<bool, StorageError> {
        let key = key.encode()?;
        let value = value.encode()?;
        match txn.put(self.db, &key, &value, WriteFlags::NO_OVERWRITE) {
            Ok(()) => Ok(true),
            Err(lmdb::Error::KeyExist) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Returns if the key was actually removed.
    pub fn remove(&self, txn: &mut RwTransaction, key: &K) -> Result<bool, StorageError> {
        let key = key.encode()?;
        match txn.del(self.db, &key, None) {
            Ok(()) => Ok(true),
            Err(lmdb::Error::NotFound) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn clear(&self, txn: &mut RwTransaction) -> Result<(), StorageError> {
        txn.clear_db(self.db).map_err(Into::into)
    }

    pub fn iter<'txn, T: Transaction>(
        &self,
        txn: &'txn T,
    ) -> Result<Iterator<'txn, RoCursor<'txn>, K, V>, StorageError> {
        let cursor = txn.open_ro_cursor(self.db)?;
        Iterator::new(cursor, Bound::Unbounded, true)
    }

    pub fn keys<'txn, T: Transaction>(
        &self,
        txn: &'txn T,
    ) -> Result<KeyIterator<'txn, RoCursor<'txn>, K>, StorageError> {
        let cursor = txn.open_ro_cursor(self.db)?;
        KeyIterator::new(cursor, Bound::Unbounded, true)
    }

    pub fn values<'txn, T: Transaction>(
        &self,
        txn: &'txn T,
    ) -> Result<ValueIterator<'txn, RoCursor<'txn>, V>, StorageError> {
        let cursor = txn.open_ro_cursor(self.db)?;
        ValueIterator::new::<K>(cursor, Bound::Unbounded, true)
    }
}

impl<'a, K: LmdbKey + 'a + ?Sized, V: LmdbValue + 'a + ?Sized> LmdbMap<K, V> {
    /// Extend the map with the contents of an iterator.
    ///
    /// Keys that exist in the map before insertion are ignored.
    pub fn extend(
        &self,
        txn: &mut RwTransaction,
        iter: impl IntoIterator<Item = (&'a K, &'a V)>,
    ) -> Result<(), StorageError> {
        for (key, value) in iter {
            self.insert(txn, key, value)?;
        }
        Ok(())
    }
}

pub fn database_key_flag<K: LmdbKey + ?Sized>() -> DatabaseFlags {
    match K::TYPE {
        LmdbValType::U32 => DatabaseFlags::INTEGER_KEY,
        #[cfg(target_pointer_width = "64")]
        LmdbValType::U64 => DatabaseFlags::INTEGER_KEY,
        LmdbValType::FixedSizeOtherThanU32OrUsize | LmdbValType::VariableSize => {
            DatabaseFlags::empty()
        }
    }
}

fn lmdb_stat<T: Transaction>(txn: &T, db: Database) -> Result<lmdb_sys::MDB_stat, lmdb::Error> {
    let mut stat = lmdb_sys::MDB_stat {
        ms_psize: 0,
        ms_depth: 0,
        ms_branch_pages: 0,
        ms_leaf_pages: 0,
        ms_overflow_pages: 0,
        ms_entries: 0,
    };
    let code = unsafe { lmdb_sys::mdb_stat(txn.txn(), db.dbi(), &mut stat) };
    if code == lmdb_sys::MDB_SUCCESS {
        Ok(stat)
    } else {
        Err(lmdb::Error::from_err_code(code))
    }
}

#[cfg(test)]
mod tests {
    use crate::lmdb_storage::{LmdbEnvironmentManager, LmdbEnvironmentOptions};

    use super::*;

    use tempdir::TempDir;

    #[test]
    fn test_lmdb_map() {
        let temp_dir = TempDir::new("test_lmdb_map").unwrap();
        let env = LmdbEnvironmentManager::create(
            temp_dir.path(),
            "env",
            LmdbEnvironmentOptions::default(),
        )
        .unwrap();
        let txn = env.create_txn().unwrap();
        let mut txn = txn.write();

        let map = LmdbMap::new_from_txn(&mut txn, None, true).unwrap();
        assert_eq!(map.count(txn.txn()).unwrap(), 0);

        assert!(map
            .insert(txn.txn_mut(), [1u8].as_slice(), [2u8].as_slice())
            .unwrap());
        assert_eq!(map.count(txn.txn()).unwrap(), 1);

        assert!(!map
            .insert(txn.txn_mut(), [1u8].as_slice(), [3u8].as_slice())
            .unwrap());
        assert_eq!(map.count(txn.txn()).unwrap(), 1);

        assert_eq!(
            map.get(txn.txn(), [1u8].as_slice())
                .unwrap()
                .unwrap()
                .to_owned(),
            vec![2]
        );
        assert!(map.get(txn.txn(), [2u8].as_slice()).unwrap().is_none());

        assert!(!map.remove(txn.txn_mut(), [2u8].as_slice()).unwrap());
        assert_eq!(map.count(txn.txn()).unwrap(), 1);
        assert!(map.remove(txn.txn_mut(), [1u8].as_slice()).unwrap());
        assert_eq!(map.count(txn.txn()).unwrap(), 0);
    }
}
