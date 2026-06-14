use std::path::Path;
use anyhow::{Context, Result};
use serde::Deserialize;

/// Compile-time embed of the default schema so tests and binary both resolve it
/// without depending on the working directory at runtime.
pub const DEFAULT_SCHEMA_STR: &str = include_str!("../schema/osm-default.toml");

/// A single OSM highway value → FRC/FOW mapping rule.
/// Rules are matched in order; first match wins.
/// `highway = ""` is a catch-all. Unknown tags with no matching rule are excluded.
/// `vehicular` defaults to `true`; set to `false` to exclude from the routing graph.
#[derive(Debug, Clone, Deserialize)]
pub struct OsmRule {
    pub highway: String,
    pub frc: u8,
    pub fow: u8,
    #[serde(default = "default_vehicular")]
    pub vehicular: bool,
}

fn default_vehicular() -> bool { true }

/// The complete OSM tag mapping loaded from a TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct OsmSchemaMapping {
    pub rules: Vec<OsmRule>,
    #[serde(default)]
    pub exclusions: std::collections::HashMap<String, Vec<String>>,
}

impl OsmSchemaMapping {
    /// Look up `(frc, fow, vehicular)` for a given OSM `highway` tag value.
    /// Returns `None` if no rule matches (unknown/unsupported highway type).
    pub fn lookup(&self, highway: &str) -> Option<(u8, u8, bool)> {
        for rule in &self.rules {
            if rule.highway.is_empty() || rule.highway == highway {
                return Some((rule.frc, rule.fow, rule.vehicular));
            }
        }
        None
    }

    /// Parse from an in-memory TOML string (used by tests and `load_default`).
    pub fn parse(toml_text: &str) -> Result<Self> {
        toml::from_str(toml_text).context("failed to parse OSM schema TOML")
    }

    /// Load the embedded default schema (compile-time baked in).
    pub fn load_default() -> Self {
        // The embedded string is valid TOML — unwrap is safe here.
        toml::from_str(DEFAULT_SCHEMA_STR).expect("embedded osm-default.toml is invalid TOML")
    }
}

pub fn load(path: &Path) -> Result<OsmSchemaMapping> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read OSM schema file '{}'", path.display()))?;
    OsmSchemaMapping::parse(&text)
        .with_context(|| format!("failed to parse OSM schema file '{}'", path.display()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> OsmSchemaMapping {
        OsmSchemaMapping::load_default()
    }

    #[test]
    fn motorway_frc0_fow1() {
        let (frc, fow, veh) = schema().lookup("motorway").unwrap();
        assert_eq!(frc, 0);
        assert_eq!(fow, 1);
        assert!(veh);
    }

    #[test]
    fn motorway_link_frc1_slip_road() {
        let (frc, fow, veh) = schema().lookup("motorway_link").unwrap();
        assert_eq!(frc, 1);
        assert_eq!(fow, 6);
        assert!(veh);
    }

    #[test]
    fn primary_frc2() {
        let (frc, _, _) = schema().lookup("primary").unwrap();
        assert_eq!(frc, 2);
    }

    #[test]
    fn secondary_frc3() {
        let (frc, _, _) = schema().lookup("secondary").unwrap();
        assert_eq!(frc, 3);
    }

    #[test]
    fn unclassified_frc6() {
        let (frc, _, _) = schema().lookup("unclassified").unwrap();
        assert_eq!(frc, 6);
    }

    #[test]
    fn residential_frc7_vehicular() {
        let (frc, _, veh) = schema().lookup("residential").unwrap();
        assert_eq!(frc, 7);
        assert!(veh);
    }

    #[test]
    fn pedestrian_non_vehicular() {
        let (_, _, veh) = schema().lookup("pedestrian").unwrap();
        assert!(!veh);
    }

    #[test]
    fn unknown_returns_none() {
        assert!(schema().lookup("proposed").is_none());
    }

    #[test]
    fn catchall_rule_matches_anything() {
        let schema = OsmSchemaMapping::parse("[[rules]]\nhighway = \"\"\nfrc = 7\nfow = 0\nvehicular = false\n").unwrap();
        let (frc, fow, veh) = schema.lookup("whatever").unwrap();
        assert_eq!(frc, 7);
        assert_eq!(fow, 0);
        assert!(!veh);
    }
}
