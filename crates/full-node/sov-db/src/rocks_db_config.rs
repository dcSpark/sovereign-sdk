// Adapted from Aptos-Core.
// Modified to remove serde dependency

use rockbound::rocksdb::{
    BlockBasedIndexType, BlockBasedOptions, Cache, ColumnFamilyDescriptor, DBCompressionType,
    Options,
};

pub use rockbound::{gen_rocksdb_options, RocksdbConfig};

const MIB: usize = 1024 * 1024;
const GIB: usize = 1024 * MIB;

const ROCKSDB_BYTES_PER_SYNC: u64 = MIB as u64;
const ROCKSDB_WAL_BYTES_PER_SYNC: u64 = MIB as u64;

/// Hardcoded RocksDB tuning profiles for different DB roles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RocksDbProfile {
    /// Main state DBs and other read-heavy hot-path stores.
    StateHeavy,
    /// Ledger/accessory-like DBs with balanced read/write characteristics.
    Standard,
    /// Smaller ephemeral DBs (sequencer side-db/blob queue) where memory should be bounded.
    Ephemeral,
}

#[derive(Clone, Copy, Debug)]
struct ProfileSettings {
    shared_block_cache_bytes: usize,
    db_write_buffer_size_bytes: usize,
    max_total_wal_size_bytes: u64,
    max_open_files: i32,
    enable_pipelined_write: bool,
    write_buffer_size_bytes: usize,
    max_write_buffer_number: i32,
    min_write_buffers_to_merge: i32,
    level0_file_num_compaction_trigger: i32,
    level0_slowdown_writes_trigger: i32,
    level0_stop_writes_trigger: i32,
    block_size_bytes: usize,
    metadata_block_size_bytes: usize,
    target_file_size_base_bytes: u64,
    max_bytes_for_level_base_bytes: u64,
    max_background_jobs: i32,
    max_subcompactions: u32,
    max_file_opening_threads: i32,
}

/// Generate tuned DB options for high-throughput rollup workloads.
pub fn gen_tuned_rocksdb_options(profile: RocksDbProfile, readonly: bool) -> Options {
    let settings = settings_for(profile);
    let mut db_opts = gen_rocksdb_options(&Default::default(), readonly);
    db_opts.increase_parallelism(settings.max_background_jobs);
    db_opts.set_max_background_jobs(settings.max_background_jobs);
    db_opts.set_max_open_files(settings.max_open_files);
    db_opts.set_max_file_opening_threads(settings.max_file_opening_threads);
    db_opts.set_max_subcompactions(settings.max_subcompactions);
    db_opts.set_enable_pipelined_write(settings.enable_pipelined_write);
    db_opts.set_bytes_per_sync(ROCKSDB_BYTES_PER_SYNC);
    db_opts.set_wal_bytes_per_sync(ROCKSDB_WAL_BYTES_PER_SYNC);
    db_opts.set_db_write_buffer_size(settings.db_write_buffer_size_bytes);
    db_opts.set_max_total_wal_size(settings.max_total_wal_size_bytes);
    db_opts
}

/// Generate tuned column family descriptors that share a single block cache.
pub fn gen_tuned_rocksdb_cfds(
    profile: RocksDbProfile,
    column_families: impl IntoIterator<Item = impl Into<String>>,
) -> Vec<ColumnFamilyDescriptor> {
    let settings = settings_for(profile);
    let shared_block_cache = Cache::new_lru_cache(settings.shared_block_cache_bytes);

    column_families
        .into_iter()
        .map(|cf_name| tuned_cf_descriptor(cf_name, &shared_block_cache, settings))
        .collect()
}

fn tuned_cf_descriptor(
    cf_name: impl Into<String>,
    shared_block_cache: &Cache,
    settings: ProfileSettings,
) -> ColumnFamilyDescriptor {
    let mut block_based = BlockBasedOptions::default();
    block_based.set_block_cache(shared_block_cache);
    block_based.set_bloom_filter(10.0, false);
    block_based.set_cache_index_and_filter_blocks(true);
    block_based.set_pin_l0_filter_and_index_blocks_in_cache(true);
    block_based.set_pin_top_level_index_and_filter(true);
    block_based.set_optimize_filters_for_memory(true);
    block_based.set_index_type(BlockBasedIndexType::TwoLevelIndexSearch);
    block_based.set_partition_filters(true);
    block_based.set_block_size(settings.block_size_bytes);
    block_based.set_metadata_block_size(settings.metadata_block_size_bytes);

    let mut cf_opts = Options::default();
    cf_opts.set_block_based_table_factory(&block_based);
    cf_opts.set_compression_type(DBCompressionType::Lz4);
    cf_opts.set_bottommost_compression_type(DBCompressionType::Lz4);
    cf_opts.set_write_buffer_size(settings.write_buffer_size_bytes);
    cf_opts.set_max_write_buffer_number(settings.max_write_buffer_number);
    cf_opts.set_min_write_buffer_number_to_merge(settings.min_write_buffers_to_merge);
    cf_opts.set_level_zero_file_num_compaction_trigger(settings.level0_file_num_compaction_trigger);
    cf_opts.set_level_zero_slowdown_writes_trigger(settings.level0_slowdown_writes_trigger);
    cf_opts.set_level_zero_stop_writes_trigger(settings.level0_stop_writes_trigger);
    cf_opts.set_target_file_size_base(settings.target_file_size_base_bytes);
    cf_opts.set_max_bytes_for_level_base(settings.max_bytes_for_level_base_bytes);
    cf_opts.set_level_compaction_dynamic_level_bytes(true);

    ColumnFamilyDescriptor::new(cf_name, cf_opts)
}

fn tuned_parallelism() -> i32 {
    let available = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(8);
    (available as i32).clamp(8, 32)
}

fn settings_for(profile: RocksDbProfile) -> ProfileSettings {
    let p = tuned_parallelism();
    match profile {
        RocksDbProfile::StateHeavy => ProfileSettings {
            // 60 GB / 32-core production profile:
            // keep roughly ~24 GB in RocksDB-managed memory and leave room for OS page cache and process headroom.
            shared_block_cache_bytes: 12 * GIB,
            db_write_buffer_size_bytes: 12 * GIB,
            max_total_wal_size_bytes: (16 * GIB) as u64,
            max_open_files: 50_000,
            enable_pipelined_write: true,
            write_buffer_size_bytes: 512 * MIB,
            max_write_buffer_number: 6,
            min_write_buffers_to_merge: 2,
            level0_file_num_compaction_trigger: 8,
            level0_slowdown_writes_trigger: 24,
            level0_stop_writes_trigger: 48,
            block_size_bytes: 16 * 1024,
            metadata_block_size_bytes: 4 * 1024,
            target_file_size_base_bytes: (256 * MIB) as u64,
            max_bytes_for_level_base_bytes: (4 * GIB) as u64,
            max_background_jobs: p.min(24),
            max_subcompactions: ((p / 3).max(4).min(12)) as u32,
            max_file_opening_threads: p.min(24),
        },
        RocksDbProfile::Standard => ProfileSettings {
            shared_block_cache_bytes: 2 * GIB,
            db_write_buffer_size_bytes: 4 * GIB,
            max_total_wal_size_bytes: (8 * GIB) as u64,
            max_open_files: 20_000,
            enable_pipelined_write: true,
            write_buffer_size_bytes: 256 * MIB,
            max_write_buffer_number: 4,
            min_write_buffers_to_merge: 2,
            level0_file_num_compaction_trigger: 8,
            level0_slowdown_writes_trigger: 24,
            level0_stop_writes_trigger: 48,
            block_size_bytes: 16 * 1024,
            metadata_block_size_bytes: 4 * 1024,
            target_file_size_base_bytes: (128 * MIB) as u64,
            max_bytes_for_level_base_bytes: GIB as u64,
            max_background_jobs: (p / 2).clamp(4, 12),
            max_subcompactions: ((p / 6).max(2).min(4)) as u32,
            max_file_opening_threads: (p / 2).clamp(8, 16),
        },
        RocksDbProfile::Ephemeral => ProfileSettings {
            shared_block_cache_bytes: 256 * MIB,
            db_write_buffer_size_bytes: GIB,
            max_total_wal_size_bytes: GIB as u64,
            max_open_files: 4_096,
            enable_pipelined_write: false,
            write_buffer_size_bytes: 64 * MIB,
            max_write_buffer_number: 4,
            min_write_buffers_to_merge: 1,
            level0_file_num_compaction_trigger: 8,
            level0_slowdown_writes_trigger: 24,
            level0_stop_writes_trigger: 48,
            block_size_bytes: 4 * 1024,
            metadata_block_size_bytes: 4 * 1024,
            target_file_size_base_bytes: (128 * MIB) as u64,
            max_bytes_for_level_base_bytes: GIB as u64,
            max_background_jobs: (p / 3).clamp(4, 8),
            max_subcompactions: ((p / 8).max(2).min(3)) as u32,
            max_file_opening_threads: (p / 3).clamp(8, 12),
        },
    }
}
