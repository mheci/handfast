//! Golden wire vectors captured from the KDE Connect protocol reference.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::frame::{Result, TestkitError, WirePacket};

/// One named golden packet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldenVector {
    /// File stem.
    pub name: String,
    /// Raw packet JSON.
    pub packet: Value,
}

/// Load and validate every `*.json` vector in a directory.
///
/// # Errors
///
/// Fails when the directory cannot be read, a file is malformed or no vectors
/// exist.
pub fn load_dir(path: impl AsRef<Path>) -> Result<Vec<GoldenVector>> {
    let mut vectors = Vec::new();
    for entry in fs::read_dir(path.as_ref())? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path)?;
        let packet: Value = serde_json::from_slice(&bytes)?;
        let wire: WirePacket = serde_json::from_value(packet.clone())?;
        wire.validate()?;
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| TestkitError::Protocol("vector has no file stem".to_string()))?
            .to_string();
        vectors.push(GoldenVector { name, packet });
    }
    vectors.sort_by(|left, right| left.name.cmp(&right.name));
    if vectors.is_empty() {
        return Err(TestkitError::Protocol(
            "no golden vectors found".to_string(),
        ));
    }
    Ok(vectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors")
    }

    #[test]
    fn repository_vectors_load() {
        let vectors = load_dir(vector_dir()).unwrap();
        assert!(vectors.len() >= 10);
        let names: Vec<&str> = vectors.iter().map(|vector| vector.name.as_str()).collect();
        assert!(names.contains(&"identity_phone"));
        assert!(names.contains(&"share_file"));
        assert!(names.contains(&"ping"));
    }
}
