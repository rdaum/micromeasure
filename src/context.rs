// Copyright 2026 Ryan Daum
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Stable runner identity, comparison environment, and report provenance.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

/// Run-level context retained in every new benchmark report.
///
/// `runner_id` and `environment` determine comparison compatibility.
/// `provenance` describes where the evidence came from but never participates
/// in compatibility. Callers must not place credentials or secrets in any
/// context value because the complete context is persisted in report artifacts.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ReportContext {
    #[serde(default)]
    pub runner_id: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub provenance: BTreeMap<String, String>,
}

impl ReportContext {
    /// Construct context for a stable physical runner identity.
    pub fn new(runner_id: impl Into<String>) -> Self {
        Self {
            runner_id: runner_id.into(),
            ..Self::default()
        }
    }

    pub fn with_environment(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    pub fn with_provenance(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.provenance.insert(key.into(), value.into());
        self
    }

    /// Load and semantically validate an explicit context document.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ContextError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| ContextError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let context: Self =
            serde_json::from_slice(&bytes).map_err(|source| ContextError::Malformed {
                path: path.to_path_buf(),
                source,
            })?;
        context.validate_at(Some(path))?;
        Ok(context)
    }

    pub(crate) fn resolved(
        mut self,
        default_runner_id: &str,
        default_provenance: BTreeMap<String, String>,
    ) -> Self {
        if self.runner_id.trim().is_empty() {
            self.runner_id = default_runner_id.to_string();
        }
        for (key, value) in default_provenance {
            self.provenance.entry(key).or_insert(value);
        }
        self
    }

    pub fn validate(&self) -> Result<(), ContextError> {
        self.validate_at(None)
    }

    pub(crate) fn validate_at(&self, path: Option<&Path>) -> Result<(), ContextError> {
        validate_map("environment", &self.environment, path)?;
        validate_map("provenance", &self.provenance, path)?;
        Ok(())
    }
}

fn validate_map(
    name: &str,
    values: &BTreeMap<String, String>,
    path: Option<&Path>,
) -> Result<(), ContextError> {
    for (key, value) in values {
        if key.trim().is_empty() {
            return Err(ContextError::Invalid {
                path: path.map(Path::to_path_buf),
                reason: format!("{name} keys must not be empty"),
            });
        }
        if value.trim().is_empty() {
            return Err(ContextError::Invalid {
                path: path.map(Path::to_path_buf),
                reason: format!("{name} value for {key:?} must not be empty"),
            });
        }
    }
    Ok(())
}

/// Failure to load or validate an explicitly requested context document.
#[derive(Debug)]
#[non_exhaustive]
pub enum ContextError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Malformed {
        path: PathBuf,
        source: serde_json::Error,
    },
    Invalid {
        path: Option<PathBuf>,
        reason: String,
    },
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(
                formatter,
                "failed to read report context {}: {source}",
                path.display()
            ),
            Self::Malformed { path, source } => write!(
                formatter,
                "malformed report context at {}: {source}",
                path.display()
            ),
            Self::Invalid { path, reason } => {
                if let Some(path) = path {
                    write!(
                        formatter,
                        "invalid report context at {}: {reason}",
                        path.display()
                    )
                } else {
                    write!(formatter, "invalid report context: {reason}")
                }
            }
        }
    }
}

impl Error for ContextError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Malformed { source, .. } => Some(source),
            Self::Invalid { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "micromeasure-context-{name}-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn explicit_context_is_loaded_without_discarding_fields() {
        let path = temporary_path("valid");
        fs::write(
            &path,
            r#"{
                "runner_id": "gpu-host-05",
                "environment": {"gpu": "GB300", "driver": "595.71.05"},
                "provenance": {"commit": "0123456789abcdef"}
            }"#,
        )
        .unwrap();

        let context = ReportContext::load_from_path(&path).unwrap();
        assert_eq!(context.runner_id, "gpu-host-05");
        assert_eq!(context.environment["gpu"], "GB300");
        assert_eq!(context.provenance["commit"], "0123456789abcdef");

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn explicit_context_rejects_unknown_fields() {
        let path = temporary_path("unknown-field");
        fs::write(
            &path,
            r#"{
                "runner_id": "gpu-host-05",
                "enviroment": {"gpu": "GB300"}
            }"#,
        )
        .unwrap();

        let error = ReportContext::load_from_path(&path).unwrap_err();
        assert!(error.to_string().contains("unknown field `enviroment`"));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn explicit_context_may_use_the_default_runner() {
        let path = temporary_path("default-runner");
        fs::write(
            &path,
            r#"{"environment":{"profile":"release"},"provenance":{}}"#,
        )
        .unwrap();

        let context = ReportContext::load_from_path(&path).unwrap();
        assert!(context.runner_id.is_empty());
        assert_eq!(context.environment["profile"], "release");

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn explicit_context_rejects_empty_map_values() {
        let path = temporary_path("invalid");
        fs::write(
            &path,
            r#"{"runner_id":"","environment":{"gpu":""},"provenance":{}}"#,
        )
        .unwrap();

        assert!(matches!(
            ReportContext::load_from_path(&path),
            Err(ContextError::Invalid { .. })
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn supplied_provenance_wins_over_local_defaults() {
        let mut defaults = BTreeMap::new();
        defaults.insert("commit".to_string(), "local".to_string());
        defaults.insert("rust_toolchain".to_string(), "rustc test".to_string());
        let context = ReportContext::new("runner")
            .with_provenance("commit", "trusted")
            .resolved("fallback", defaults);

        assert_eq!(context.provenance["commit"], "trusted");
        assert_eq!(context.provenance["rust_toolchain"], "rustc test");
    }

    #[test]
    fn omitted_runner_resolves_to_the_local_hostname() {
        let context = ReportContext::default().resolved("physical-node", BTreeMap::new());
        assert_eq!(context.runner_id, "physical-node");
    }
}
