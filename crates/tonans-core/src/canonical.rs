//! Canonical serialization and content hashing.
//!
//! Two runs of the same model over the same inputs must produce byte-identical
//! output. `serde_json` gets us most of the way there — its default map is a
//! `BTreeMap`, so keys sort, and its float formatter is the shortest round-trip
//! representation, which is a pure function of the bit pattern. Two things it
//! does not do for us:
//!
//! 1. `-0.0` and `0.0` are arithmetically equal but serialize differently. A
//!    subtraction that lands on zero from above or below would otherwise change a
//!    project hash. [`canonical_value`] collapses the sign.
//! 2. A non-finite float serializes as `null`, silently. That would put a `null`
//!    where a kilowatt-hour belongs in an exported report. [`canonical_value`]
//!    rejects every `null` it finds and names the path, so the failure is loud.
//!
//! Rejecting *all* nulls is only viable because the schema marks every optional
//! field `skip_serializing_if`: an absent optional disappears rather than
//! becoming `null`. That is a real constraint on the schema, and
//! `tests/units_contract.rs` holds it in place.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::units::normalize_zero;

/// Serialize `value` to a canonical [`serde_json::Value`].
///
/// Fails if the tree contains a `null`, which — given the schema's use of
/// `skip_serializing_if` — means a `NaN` or an infinity reached serialization.
pub fn canonical_value<T: Serialize>(value: &T) -> Result<serde_json::Value> {
    let raw = serde_json::to_value(value)
        .map_err(|e| Error::json("serializing for canonical form", e))?;
    let mut normalized = raw;
    normalize(&mut normalized, "$")?;
    Ok(normalized)
}

/// Serialize `value` to canonical JSON text, with sorted keys and no whitespace.
///
/// This is the exact byte sequence that gets hashed. It is not the same as the
/// pretty-printed form written to disk, which is generated for a human to read.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<String> {
    let value = canonical_value(value)?;
    serde_json::to_string(&value).map_err(|e| Error::json("writing canonical json", e))
}

/// Hash `value`'s canonical form.
pub fn canonical_hash<T: Serialize>(value: &T) -> Result<Hash> {
    Ok(hash_bytes(canonical_json(value)?.as_bytes()))
}

/// Hash raw bytes.
///
/// Used for imported files, where the hash must be of exactly what the user
/// handed us, before any parsing or interpretation.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Hash(hex(&hasher.finalize()))
}

/// A lowercase hex SHA-256 digest, prefixed with its algorithm.
///
/// The prefix is not decoration: an unlabelled 64-character hex string in a
/// provenance manifest is not self-describing, and a future change of algorithm
/// would silently produce a value that looks identical in kind.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Hash(String);

impl Hash {
    /// The digest as `sha256:<64 hex chars>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The bare hex digest, without the algorithm prefix.
    ///
    /// This is the form written into `SHA256SUMS.txt`, which must stay compatible
    /// with `sha256sum -c` and `Get-FileHash`.
    #[must_use]
    pub fn bare_hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or(&self.0)
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(7 + bytes.len() * 2);
    out.push_str("sha256:");
    for byte in bytes {
        // Deliberately hand-rolled: a hex crate would be one more line in the
        // SBOM for sixteen characters of lookup table.
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn normalize(value: &mut serde_json::Value, path: &str) -> Result<()> {
    match value {
        serde_json::Value::Null => Err(Error::non_finite(path, f64::NAN)),
        serde_json::Value::Number(number) => {
            if let Some(float) = number.as_f64() {
                if !float.is_finite() {
                    return Err(Error::non_finite(path, float));
                }
                if float == 0.0 && float.is_sign_negative() {
                    *number = serde_json::Number::from_f64(normalize_zero(float))
                        .ok_or_else(|| Error::non_finite(path, float))?;
                }
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                normalize(item, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                normalize(item, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        serde_json::Value::Bool(_) | serde_json::Value::String(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Sample {
        zebra_kwh: f64,
        alpha_kwh: f64,
    }

    #[test]
    fn keys_are_sorted_regardless_of_declaration_order() {
        let json = canonical_json(&Sample {
            zebra_kwh: 1.0,
            alpha_kwh: 2.0,
        })
        .unwrap();
        assert_eq!(json, r#"{"alpha_kwh":2.0,"zebra_kwh":1.0}"#);
    }

    #[test]
    fn negative_zero_hashes_the_same_as_positive_zero() {
        // 0.3 - 0.3 lands on +0.0; -0.0 arises from e.g. (-1.0) * 0.0. Both are
        // "no energy", and both must produce the same project hash.
        let positive = canonical_hash(&serde_json::json!({ "x_kwh": 0.0 })).unwrap();
        let negative = canonical_hash(&serde_json::json!({ "x_kwh": -0.0 })).unwrap();
        assert_eq!(positive, negative);
    }

    #[test]
    fn non_finite_values_are_rejected_rather_than_serialized_as_null() {
        // serde_json turns NaN into `null` without complaint. That is precisely
        // the failure mode this module exists to prevent.
        #[derive(Serialize)]
        struct Broken {
            soc_pct: f64,
        }
        assert_eq!(
            serde_json::to_string(&Broken { soc_pct: f64::NAN }).unwrap(),
            r#"{"soc_pct":null}"#
        );

        let error = canonical_json(&Broken { soc_pct: f64::NAN }).unwrap_err();
        assert_eq!(error.code(), "internal.non_finite");
        assert!(error.to_string().contains("soc_pct"));

        let error = canonical_json(&Broken {
            soc_pct: f64::INFINITY,
        })
        .unwrap_err();
        assert_eq!(error.code(), "internal.non_finite");
    }

    #[test]
    fn nulls_nested_in_arrays_are_found_and_named() {
        let error =
            canonical_json(&serde_json::json!({ "rows": [{ "a_kwh": 1.0 }, { "a_kwh": null }] }))
                .unwrap_err();
        assert!(error.to_string().contains("$.rows[1].a_kwh"), "{error}");
    }

    #[test]
    fn hash_is_stable_and_self_describing() {
        let hash = hash_bytes(b"tonans");
        assert!(hash.as_str().starts_with("sha256:"));
        assert_eq!(hash.bare_hex().len(), 64);
        assert!(hash.bare_hex().chars().all(|c| c.is_ascii_hexdigit()));
        // Fixed vector: guards against an accidental change of algorithm or encoding.
        assert_eq!(
            hash.as_str(),
            "sha256:510e5bab88a292bb0b82b4a0c38993a1937658e1280f82bf7ab6d70effaacdf0"
        );
    }

    #[test]
    fn identical_content_hashes_identically_across_construction_order() {
        let a = canonical_hash(&serde_json::json!({ "b": 2, "a": 1 })).unwrap();
        let b = canonical_hash(&serde_json::json!({ "a": 1, "b": 2 })).unwrap();
        assert_eq!(a, b);
    }
}
