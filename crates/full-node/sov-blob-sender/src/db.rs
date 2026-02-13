use std::path::Path;
use std::sync::Arc;

use rockbound::SchemaBatch;
use sov_db::rocks_db_config::{gen_tuned_rocksdb_cfds, gen_tuned_rocksdb_options, RocksDbProfile};
use sov_modules_api::DaSpec;

use crate::{BlobExecutionStatus, BlobInternalId, BlobSubmissionRequest, BlobSubmissionStatus};

const ROCKSDB_STATS_DUMP_PERIOD_SEC: u32 = 10;

#[derive(Debug)]
pub struct BlobSenderDb {
    db: rockbound::DB,
}

impl BlobSenderDb {
    const DB_NAME: &'static str = "blob_sender";
    const TABLES: &'static [&'static str] =
        &[tables::Blobs::table_name(), tables::BlobInfos::table_name()];

    pub async fn new(path: &Path) -> anyhow::Result<Self> {
        let mut db_options = gen_tuned_rocksdb_options(RocksDbProfile::Ephemeral, false);
        db_options.enable_statistics();
        db_options.set_stats_dump_period_sec(ROCKSDB_STATS_DUMP_PERIOD_SEC);
        let cf_descriptors =
            gen_tuned_rocksdb_cfds(RocksDbProfile::Ephemeral, Self::TABLES.iter().copied());
        let db = rockbound::DB::open_with_cfds(
            &db_options,
            path.join(Self::DB_NAME),
            Self::DB_NAME,
            cf_descriptors,
            0, // We keep rockbound's key/value cache disabled for this DB.
        )?;

        Ok(Self { db })
    }

    pub async fn get_all<Da: DaSpec>(&self) -> anyhow::Result<Vec<BlobSubmissionRequest<Da>>> {
        let mut blobs = vec![];

        for iter_res in self.db.iter::<tables::Blobs>()? {
            let item = iter_res?;
            let blob_id = item.key;
            let blob = item.value;

            let latest_known_processing_state = self
                .db
                .get_async::<tables::BlobInfos>(&blob_id)
                .await?
                .map(|blob_info| blob_info.blob_execution_status::<Da>())
                // If we're missing blob state information, we will just assume
                // we must resubmit it.
                .unwrap_or(BlobExecutionStatus {
                    blob_submission_status: BlobSubmissionStatus::MustSubmit,
                    blob_selector_status: None,
                });

            blobs.push(BlobSubmissionRequest {
                blob,
                blob_id,
                latest_known_processing_state,
            });
        }

        Ok(blobs)
    }

    pub async fn push(&self, blob: BlobToSend, id: BlobInternalId) -> anyhow::Result<()> {
        self.db.put_async::<tables::Blobs>(&id, &blob).await?;

        Ok(())
    }

    pub async fn set_state<Da: DaSpec>(
        &self,
        blob_id: BlobInternalId,
        state: &BlobExecutionStatus<Da>,
    ) -> anyhow::Result<()> {
        self.db
            .put_async::<tables::BlobInfos>(&blob_id, &BlobInfo::new(state))
            .await?;

        Ok(())
    }

    pub async fn remove(&self, id: BlobInternalId) -> anyhow::Result<()> {
        let mut s = SchemaBatch::new();

        s.delete::<tables::Blobs>(&id)?;
        s.delete::<tables::BlobInfos>(&id)?;

        self.db.write_schemas_async(s).await?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub enum BlobToSend {
    Batch { data: Arc<[u8]> },
    Proof { data: Arc<[u8]> },
}

impl BlobToSend {
    pub fn data(&self) -> &[u8] {
        match self {
            BlobToSend::Batch { data } | BlobToSend::Proof { data } => data.as_ref(),
        }
    }
}

// This shouldn't be public, but `define_table_...` complains if it isn't.
#[derive(Debug, Clone, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct BlobInfo {
    json_serialized_state: Vec<u8>,
}

impl BlobInfo {
    fn new<Da: DaSpec>(blob_execution_status: &BlobExecutionStatus<Da>) -> Self {
        Self {
            json_serialized_state: serde_json::to_vec(blob_execution_status)
                .expect("Failed to serialize blob processing state"),
        }
    }

    fn blob_execution_status<Da: DaSpec>(&self) -> BlobExecutionStatus<Da> {
        serde_json::from_slice(&self.json_serialized_state)
            .expect("Invalid blob info in the database")
    }
}

mod tables {
    use sov_db::{
        define_table_with_seek_key_codec, define_table_without_codec, impl_borsh_value_codec,
    };

    use super::*;

    define_table_with_seek_key_codec!(
        (Blobs) BlobInternalId => BlobToSend
    );

    define_table_with_seek_key_codec!(
        (BlobInfos) BlobInternalId => BlobInfo
    );
}
