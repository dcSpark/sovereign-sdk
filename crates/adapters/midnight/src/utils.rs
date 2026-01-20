use anyhow::{Context, Result};
use serde_json::Value;

use crate::BridgeLedger;

/// Pretty-print the full `BridgeLedger` in a normalized JSON layout.
pub fn dump_bridge_ledger(ledger: &BridgeLedger) -> Result<()> {
    let json_value = serde_json::to_value(ledger).context("failed to serialize bridge ledger")?;
    let normalized = convert_byte_arrays(json_value);
    let mut buffer = String::new();
    format_json_with_inline_arrays(&normalized, 0, &mut buffer)?;
    println!("Parsed ledger:\n{}", buffer);
    Ok(())
}

fn format_json_with_inline_arrays(value: &Value, indent: usize, out: &mut String) -> Result<()> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            let rendered =
                serde_json::to_string(value).context("failed to render scalar json value")?;
            out.push_str(&rendered);
        }
        Value::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push_str(", ");
                }
                format_json_with_inline_arrays(item, indent, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push_str("{\n");
            for (idx, (key, item)) in map.iter().enumerate() {
                write_indent(indent + 2, out);
                let rendered_key =
                    serde_json::to_string(key).context("failed to render json key")?;
                out.push_str(&rendered_key);
                out.push_str(": ");
                format_json_with_inline_arrays(item, indent + 2, out)?;
                if idx + 1 != map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_indent(indent, out);
            out.push('}');
        }
    }
    Ok(())
}

fn write_indent(level: usize, out: &mut String) {
    for _ in 0..level {
        out.push(' ');
    }
}

fn convert_byte_arrays(value: Value) -> Value {
    match value {
        Value::Array(items) => {
            if is_byte_array(&items) {
                let bytes: Vec<u8> = items
                    .into_iter()
                    .map(|item| item.as_u64().unwrap() as u8)
                    .collect();
                Value::String(hex::encode(bytes))
            } else {
                Value::Array(items.into_iter().map(convert_byte_arrays).collect())
            }
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, val)| (key, convert_byte_arrays(val)))
                .collect(),
        ),
        other => other,
    }
}

fn is_byte_array(items: &[Value]) -> bool {
    !items.is_empty()
        && items.iter().all(|item| match item {
            Value::Number(num) => num
                .as_u64()
                .map(|val| val <= u8::MAX as u64)
                .unwrap_or(false),
            _ => false,
        })
}
