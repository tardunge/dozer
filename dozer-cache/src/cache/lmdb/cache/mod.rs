use std::collections::HashMap;
use std::fmt::Debug;
use std::path::PathBuf;

use dozer_storage::lmdb::{RoTransaction, RwTransaction, Transaction};
use dozer_storage::lmdb_storage::{
    LmdbEnvironmentManager, LmdbExclusiveTransaction, SharedTransaction,
};
use dozer_storage::{LmdbMap, LmdbMultimap};

use dozer_types::node::{NodeHandle, OpIdentifier, SourceStates};
use dozer_types::parking_lot::RwLockReadGuard;

use dozer_types::types::{Field, FieldType, IndexDefinition, Record};
use dozer_types::types::{Schema, SchemaIdentifier};

use self::id_database::get_or_generate_id;
use self::secondary_index_database::{
    new_secondary_index_database_from_env, new_secondary_index_database_from_txn,
};

use super::super::{RoCache, RwCache};
use super::indexer::Indexer;
use super::utils::{self, CacheReadOptions};
use super::utils::{CacheOptions, CacheOptionsKind};
use crate::cache::expression::QueryExpression;
use crate::cache::index::get_primary_key;
use crate::cache::RecordWithId;
use crate::errors::CacheError;
use query::LmdbQueryHandler;

mod helper;
mod id_database;
mod query;
mod schema_database;
mod secondary_index_database;

use schema_database::SchemaDatabase;

pub type SecondaryIndexDatabases = HashMap<(SchemaIdentifier, usize), LmdbMultimap<[u8], u64>>;

#[derive(Clone, Debug)]
pub struct CacheCommonOptions {
    // Total number of readers allowed
    pub max_readers: u32,
    // Max no of dbs
    pub max_db_size: u32,

    /// The chunk size when calculating intersection of index queries.
    pub intersection_chunk_size: usize,

    /// Provide a path where db will be created. If nothing is provided, will default to a temp location.
    /// Db path will be `PathBuf.join(String)`.
    pub path: Option<(PathBuf, String)>,
}

impl Default for CacheCommonOptions {
    fn default() -> Self {
        Self {
            max_readers: 1000,
            max_db_size: 1000,
            intersection_chunk_size: 100,
            path: None,
        }
    }
}

#[derive(Debug)]
pub struct LmdbRoCache {
    common: LmdbCacheCommon,
    env: LmdbEnvironmentManager,
}

impl LmdbRoCache {
    pub fn new(options: CacheCommonOptions) -> Result<Self, CacheError> {
        let (mut env, name) = utils::init_env(&CacheOptions {
            common: options.clone(),
            kind: CacheOptionsKind::ReadOnly(CacheReadOptions {}),
        })?;
        let common = LmdbCacheCommon::new(&mut env, options, name, false)?;
        Ok(Self { common, env })
    }
}

#[derive(Clone, Debug)]
pub struct CacheWriteOptions {
    // Total size allocated for data in a memory mapped file.
    // This size is allocated at initialization.
    pub max_size: usize,
}

impl Default for CacheWriteOptions {
    fn default() -> Self {
        Self {
            max_size: 1024 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct LmdbRwCache {
    common: LmdbCacheCommon,
    checkpoint_db: LmdbMap<NodeHandle, OpIdentifier>,
    txn: SharedTransaction,
}

impl LmdbRwCache {
    pub fn create(
        schemas: impl IntoIterator<Item = (String, Schema, Vec<IndexDefinition>)>,
        common_options: CacheCommonOptions,
        write_options: CacheWriteOptions,
    ) -> Result<Self, CacheError> {
        let mut cache = Self::open(common_options, write_options)?;

        let mut txn = cache.txn.write();
        for (schema_name, schema, secondary_indexes) in schemas {
            cache
                .common
                .insert_schema(&mut txn, schema_name, schema, secondary_indexes)?;
        }

        txn.commit_and_renew()?;
        drop(txn);

        Ok(cache)
    }

    pub fn open(
        common_options: CacheCommonOptions,
        write_options: CacheWriteOptions,
    ) -> Result<Self, CacheError> {
        let (mut env, name) = utils::init_env(&CacheOptions {
            common: common_options.clone(),
            kind: CacheOptionsKind::Write(write_options),
        })?;
        let common = LmdbCacheCommon::new(&mut env, common_options, name, true)?;
        let checkpoint_db = LmdbMap::new_from_env(&mut env, Some("checkpoint"), true)?;
        let txn = env.create_txn()?;
        Ok(Self {
            common,
            checkpoint_db,
            txn,
        })
    }
}

impl<C: LmdbCache> RoCache for C {
    fn name(&self) -> &str {
        &self.common().name
    }

    fn get(&self, key: &[u8]) -> Result<RecordWithId, CacheError> {
        let txn = self.begin_txn()?;
        let txn = txn.as_txn();
        let id = self
            .common()
            .primary_key_to_record_id
            .get(txn, key)?
            .ok_or(CacheError::PrimaryKeyNotFound)?
            .into_owned();
        let record = self
            .common()
            .record_id_to_record
            .get(txn, &id)?
            .ok_or(CacheError::PrimaryKeyNotFound)?
            .into_owned();
        Ok(RecordWithId::new(id, record))
    }

    fn count(&self, schema_name: &str, query: &QueryExpression) -> Result<usize, CacheError> {
        let txn = self.begin_txn()?;
        let txn = txn.as_txn();
        let (schema, secondary_indexes) = self
            .common()
            .schema_db
            .get_schema_from_name(schema_name)
            .ok_or_else(|| CacheError::SchemaNotFound(schema_name.to_string()))?;
        let handler = LmdbQueryHandler::new(self.common(), txn, schema, secondary_indexes, query);
        handler.count()
    }

    fn query(
        &self,
        schema_name: &str,
        query: &QueryExpression,
    ) -> Result<(&Schema, Vec<RecordWithId>), CacheError> {
        let txn = self.begin_txn()?;
        let txn = txn.as_txn();
        let (schema, secondary_indexes) = self
            .common()
            .schema_db
            .get_schema_from_name(schema_name)
            .ok_or_else(|| CacheError::SchemaNotFound(schema_name.to_string()))?;
        let handler = LmdbQueryHandler::new(self.common(), txn, schema, secondary_indexes, query);
        let records = handler.query()?;
        Ok((schema, records))
    }

    fn get_schema_and_indexes_by_name(
        &self,
        name: &str,
    ) -> Result<&(Schema, Vec<IndexDefinition>), CacheError> {
        let schema = self
            .common()
            .schema_db
            .get_schema_from_name(name)
            .ok_or_else(|| CacheError::SchemaNotFound(name.to_string()))?;
        Ok(schema)
    }

    fn get_schema(&self, schema_identifier: SchemaIdentifier) -> Result<&Schema, CacheError> {
        self.common()
            .schema_db
            .get_schema(schema_identifier)
            .map(|(schema, _)| schema)
            .ok_or(CacheError::SchemaIdentifierNotFound(schema_identifier))
    }
}

impl RwCache for LmdbRwCache {
    fn insert(&self, record: &mut Record) -> Result<u64, CacheError> {
        let (schema, secondary_indexes) = self.get_schema_and_indexes_from_record(record)?;
        record.version = Some(INITIAL_RECORD_VERSION);
        self.insert_impl(record, schema, secondary_indexes)
    }

    fn delete(&self, key: &[u8]) -> Result<u32, CacheError> {
        let (_, _, version) = self.delete_impl(key)?;
        Ok(version)
    }

    fn update(&self, key: &[u8], record: &mut Record) -> Result<u32, CacheError> {
        let (schema, secondary_indexes, old_version) = self.delete_impl(key)?;
        record.version = Some(old_version + 1);
        self.insert_impl(record, schema, secondary_indexes)?;
        Ok(old_version)
    }

    fn commit(&self, checkpoint: &SourceStates) -> Result<(), CacheError> {
        let mut txn = self.txn.write();
        self.checkpoint_db.clear(txn.txn_mut())?;
        self.checkpoint_db.extend(txn.txn_mut(), checkpoint)?;
        txn.commit_and_renew()?;
        Ok(())
    }

    fn get_checkpoint(&self) -> Result<SourceStates, CacheError> {
        let txn = self.txn.read();
        let result = self
            .checkpoint_db
            .iter(txn.txn())?
            .map(|result| {
                result
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .map_err(CacheError::Storage)
            })
            .collect();
        result
    }
}

impl LmdbRwCache {
    fn delete_impl(&self, key: &[u8]) -> Result<(&Schema, &[IndexDefinition], u32), CacheError> {
        let record = self.get(key)?;
        let (schema, secondary_indexes) =
            self.get_schema_and_indexes_from_record(&record.record)?;

        let mut txn = self.txn.write();
        let txn = txn.txn_mut();

        if !self.common.record_id_to_record.remove(txn, &record.id)? {
            panic!("We just got this key from the map");
        }

        let indexer = Indexer {
            secondary_indexes: &self.common.secondary_indexes,
        };
        indexer.delete_indexes(txn, &record.record, schema, secondary_indexes, record.id)?;
        let version = record
            .record
            .version
            .expect("All records in cache should have a version");
        Ok((schema, secondary_indexes, version))
    }

    fn insert_impl(
        &self,
        record: &Record,
        schema: &Schema,
        secondary_indexes: &[IndexDefinition],
    ) -> Result<u64, CacheError> {
        let mut txn = self.txn.write();
        let txn = txn.txn_mut();

        let id = if schema.primary_index.is_empty() {
            get_or_generate_id(self.common.primary_key_to_record_id, txn, None)?
        } else {
            let primary_key = get_primary_key(&schema.primary_index, &record.values);
            get_or_generate_id(
                self.common.primary_key_to_record_id,
                txn,
                Some(&primary_key),
            )?
        };
        if !self.common.record_id_to_record.insert(txn, &id, record)? {
            return Err(CacheError::PrimaryKeyExists);
        }

        let indexer = Indexer {
            secondary_indexes: &self.common.secondary_indexes,
        };

        indexer.build_indexes(txn, record, schema, secondary_indexes, id)?;

        Ok(id)
    }
}

/// This trait abstracts the behavior of getting a transaction from a `LmdbExclusiveTransaction` or a `lmdb::Transaction`.
trait AsTransaction {
    type Transaction<'a>: Transaction
    where
        Self: 'a;

    fn as_txn(&self) -> &Self::Transaction<'_>;
}

impl<'a> AsTransaction for RoTransaction<'a> {
    type Transaction<'env> = RoTransaction<'env> where Self: 'env;

    fn as_txn(&self) -> &Self::Transaction<'_> {
        self
    }
}

impl<'a> AsTransaction for RwLockReadGuard<'a, LmdbExclusiveTransaction> {
    type Transaction<'env> = RwTransaction<'env> where Self: 'env;

    fn as_txn(&self) -> &Self::Transaction<'_> {
        self.txn()
    }
}

/// This trait abstracts the behavior of locking a `SharedTransaction` for reading
/// and beginning a `RoTransaction` from `LmdbEnvironmentManager`.
trait LmdbCache: Send + Sync + Debug {
    type AsTransaction<'a>: AsTransaction
    where
        Self: 'a;

    fn common(&self) -> &LmdbCacheCommon;
    fn begin_txn(&self) -> Result<Self::AsTransaction<'_>, CacheError>;

    fn get_schema_and_indexes_from_record(
        &self,
        record: &Record,
    ) -> Result<&(Schema, Vec<IndexDefinition>), CacheError> {
        let schema_identifier = record.schema_id.ok_or(CacheError::SchemaHasNoIdentifier)?;
        let schema = self
            .common()
            .schema_db
            .get_schema(schema_identifier)
            .ok_or(CacheError::SchemaIdentifierNotFound(schema_identifier))?;

        debug_check_schema_record_consistency(&schema.0, record);

        Ok(schema)
    }
}

impl LmdbCache for LmdbRoCache {
    type AsTransaction<'a> = RoTransaction<'a>;

    fn common(&self) -> &LmdbCacheCommon {
        &self.common
    }

    fn begin_txn(&self) -> Result<Self::AsTransaction<'_>, CacheError> {
        Ok(self.env.begin_ro_txn()?)
    }
}

impl LmdbCache for LmdbRwCache {
    type AsTransaction<'a> = RwLockReadGuard<'a, LmdbExclusiveTransaction>;

    fn common(&self) -> &LmdbCacheCommon {
        &self.common
    }

    fn begin_txn(&self) -> Result<Self::AsTransaction<'_>, CacheError> {
        Ok(self.txn.read())
    }
}

fn debug_check_schema_record_consistency(schema: &Schema, record: &Record) {
    debug_assert_eq!(schema.identifier, record.schema_id);
    debug_assert_eq!(schema.fields.len(), record.values.len());
    for (field, value) in schema.fields.iter().zip(record.values.iter()) {
        if field.nullable && value == &Field::Null {
            continue;
        }
        match field.typ {
            FieldType::UInt => {
                debug_assert!(value.as_uint().is_some())
            }
            FieldType::Int => {
                debug_assert!(value.as_int().is_some())
            }
            FieldType::Float => {
                debug_assert!(value.as_float().is_some())
            }
            FieldType::Boolean => debug_assert!(value.as_boolean().is_some()),
            FieldType::String => debug_assert!(value.as_string().is_some()),
            FieldType::Text => debug_assert!(value.as_text().is_some()),
            FieldType::Binary => debug_assert!(value.as_binary().is_some()),
            FieldType::Decimal => debug_assert!(value.as_decimal().is_some()),
            FieldType::Timestamp => debug_assert!(value.as_timestamp().is_some()),
            FieldType::Date => debug_assert!(value.as_date().is_some()),
            FieldType::Bson => debug_assert!(value.as_bson().is_some()),
            FieldType::Point => debug_assert!(value.as_point().is_some()),
        }
    }
}

const INITIAL_RECORD_VERSION: u32 = 1_u32;

#[derive(Debug)]
pub struct LmdbCacheCommon {
    record_id_to_record: LmdbMap<u64, Record>,
    primary_key_to_record_id: LmdbMap<[u8], u64>,
    secondary_indexes: SecondaryIndexDatabases,
    schema_db: SchemaDatabase,
    cache_options: CacheCommonOptions,
    /// File name of the database.
    name: String,
}

impl LmdbCacheCommon {
    fn new(
        env: &mut LmdbEnvironmentManager,
        options: CacheCommonOptions,
        name: String,
        create_db_if_not_exist: bool,
    ) -> Result<Self, CacheError> {
        // Create or open must have databases.
        let record_id_to_record =
            LmdbMap::new_from_env(env, Some("records"), create_db_if_not_exist)?;
        let primary_key_to_record_id =
            LmdbMap::new_from_env(env, Some("primary_index"), create_db_if_not_exist)?;
        let schema_db = SchemaDatabase::new(env, create_db_if_not_exist)?;

        // Open existing secondary index databases.
        let mut secondary_indexe_databases = HashMap::default();
        for (schema, secondary_indexes) in schema_db.get_all_schemas() {
            let schema_id = schema.identifier.ok_or(CacheError::SchemaHasNoIdentifier)?;
            for (index, index_definition) in secondary_indexes.iter().enumerate() {
                let db = new_secondary_index_database_from_env(
                    env,
                    &schema_id,
                    index,
                    index_definition,
                    false,
                )?;
                secondary_indexe_databases.insert((schema_id, index), db);
            }
        }

        Ok(Self {
            record_id_to_record,
            primary_key_to_record_id,
            secondary_indexes: secondary_indexe_databases,
            schema_db,
            cache_options: options,
            name,
        })
    }

    fn insert_schema(
        &mut self,
        txn: &mut LmdbExclusiveTransaction,
        schema_name: String,
        schema: Schema,
        secondary_indexes: Vec<IndexDefinition>,
    ) -> Result<(), CacheError> {
        let schema_id = schema.identifier.ok_or(CacheError::SchemaHasNoIdentifier)?;
        for (index, index_definition) in secondary_indexes.iter().enumerate() {
            let db = new_secondary_index_database_from_txn(
                txn,
                &schema_id,
                index,
                index_definition,
                true,
            )?;
            self.secondary_indexes.insert((schema_id, index), db);
        }

        self.schema_db
            .insert(txn.txn_mut(), schema_name, schema, secondary_indexes)?;
        Ok(())
    }
}

/// Methods for testing.
#[cfg(test)]
mod tests {
    use super::*;

    impl LmdbRwCache {
        pub fn get_txn_and_secondary_indexes(
            &self,
        ) -> (&SharedTransaction, &SecondaryIndexDatabases) {
            (&self.txn, &self.common.secondary_indexes)
        }
    }
}
