//! Ledger statistics collection for e2e_runner.
//!
//! Helpers for collecting gas usage and batch size statistics from the ledger.

use std::collections::{BTreeSet, HashMap};

use anyhow::{bail, Context, Result};
use serde_json::Value as JsonValue;
use sov_api_spec::types as api_types;
use sov_modules_api::Spec;
use sov_node_client::NodeClient;

use crate::bench_shared::DemoRollupSpec;

/// Gas type for the demo rollup.
pub(crate) type DemoGas = <DemoRollupSpec as Spec>::Gas;

/// Collect gas usage statistics for a set of batches.
pub(crate) async fn collect_batch_gas_stats(
    client: &NodeClient,
    batch_ids: &BTreeSet<u64>,
) -> Result<HashMap<u64, DemoGas>> {
    let mut per_batch = HashMap::new();

    for batch_id in batch_ids {
        let endpoint = format!("/ledger/batches/{}?children=1", batch_id);
        match client
            .query_rest_endpoint::<api_types::LedgerBatch>(&endpoint)
            .await
        {
            Ok(batch) => match decode_batch_gas(&batch.receipt) {
                Ok(Some(gas)) => {
                    per_batch.insert(*batch_id, gas);
                }
                Ok(None) => {
                    eprintln!(
                        "[gas] Batch {} did not expose gas data in its receipt payload",
                        batch_id
                    );
                }
                Err(err) => {
                    eprintln!(
                        "[gas] Failed to parse gas data for batch {}: {err:?}",
                        batch_id
                    );
                }
            },
            Err(err) => {
                eprintln!(
                    "[gas] Failed to fetch batch {} from ledger: {err:?}",
                    batch_id
                );
            }
        }
    }

    Ok(per_batch)
}

fn decode_batch_gas(receipt: &api_types::AnyJsonValue) -> Result<Option<DemoGas>> {
    let value = any_json_to_value(receipt);
    if let Some(obj) = value.as_object() {
        if let Some(gas_value) = obj.get("gas_used") {
            return Ok(Some(gas_from_array(gas_value)?));
        }
    }
    Ok(None)
}

/// Collect serialized batch sizes for a set of batches.
/// 
/// Computes batch size as enforced by BatchSizeTracker:
/// size = 8 (sequence_number) + 1 (visible_slots_to_advance) + 4 (tx vec len)
///      + sum_over_txs(4 (borsh vec elem overhead) + tx_body.len())
pub(crate) async fn collect_batch_sizes(
    client: &NodeClient,
    batch_ids: &BTreeSet<u64>,
) -> Result<HashMap<u64, usize>> {
    let mut per_batch = HashMap::new();
    for batch_id in batch_ids {
        let endpoint = format!("/ledger/batches/{}?children=1", batch_id);
        match client
            .query_rest_endpoint::<api_types::LedgerBatch>(&endpoint)
            .await
        {
            Ok(batch) => {
                let mut total: usize = 8 + 1 + 4; // overhead
                for tx in &batch.txs {
                    total += 4 + tx.body.len();
                }
                per_batch.insert(*batch_id, total);
            }
            Err(err) => {
                eprintln!(
                    "[bytes] Failed to fetch batch {} from ledger: {err:?}",
                    batch_id
                );
            }
        }
    }
    Ok(per_batch)
}

fn any_json_to_value(value: &api_types::AnyJsonValue) -> JsonValue {
    match value {
        api_types::AnyJsonValue::String(s) => JsonValue::String(s.clone()),
        api_types::AnyJsonValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        api_types::AnyJsonValue::Boolean(b) => JsonValue::Bool(*b),
        api_types::AnyJsonValue::Array(values) => JsonValue::Array(values.clone()),
        api_types::AnyJsonValue::Object(map) => JsonValue::Object(map.clone()),
    }
}

fn gas_from_array(value: &JsonValue) -> Result<DemoGas> {
    let array = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("gas_used must be an array"))?;
    let mut limbs = Vec::with_capacity(array.len());
    for item in array {
        if let Some(num) = item.as_u64() {
            limbs.push(num);
        } else if let Some(text) = item.as_str() {
            limbs.push(text.parse::<u64>().context("Failed to parse gas limb")?);
        } else {
            bail!("gas limb must be a number");
        }
    }
    DemoGas::try_from(limbs).context("Failed to construct gas value")
}

/// Format gas value for display.
pub(crate) fn format_gas(gas: &DemoGas) -> String {
    let limbs: Vec<String> = gas.as_ref().iter().map(|v| v.to_string()).collect();
    format!("[{}]", limbs.join(", "))
}
