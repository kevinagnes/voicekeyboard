use serde::{Deserialize, Serialize};

pub const REGISTRY_JSON: &str = include_str!("../models/registry.json");

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub filename: String,
    pub url: String,
    pub checksum_sha256: String,
    pub size_bytes: u64,
    pub default: bool,
    pub languages: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryFile {
    pub models: Vec<ModelInfo>,
}

#[derive(Clone, Debug, Default)]
pub struct ModelRegistry {
    models: Vec<ModelInfo>,
}

impl ModelRegistry {
    pub fn from_embedded() -> Self {
        let registry: RegistryFile =
            serde_json::from_str(REGISTRY_JSON).expect("embedded model registry is valid");
        Self {
            models: registry.models,
        }
    }

    pub fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    pub fn find(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.id == id)
    }

    #[cfg(test)]
    pub fn default_model(&self) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.default)
    }

    pub fn download_dir(&self) -> std::path::PathBuf {
        crate::app_data_dir().join("models")
    }

    pub fn resolve_path(&self, id: &str) -> Option<std::path::PathBuf> {
        self.find(id).map(|m| self.download_dir().join(&m.filename))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_parses_and_has_default() {
        let r = ModelRegistry::from_embedded();
        assert!(!r.models().is_empty());
        assert!(r.default_model().is_some());
        assert!(r.default_model().unwrap().default);
        assert!(r.models().iter().all(|m| !m.id.is_empty()));
    }
}
