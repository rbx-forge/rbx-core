//! Owner (user or group) of a Roblox project. Centralized so every tool
//! agrees on serialization and resolution.
//!
//! The shape mirrors what users already write in tool-specific configs
//! (e.g. `[creator]` in `rbxshop.toml`) and what we want as a single
//! source of truth at `[owner]` in `rbxplace.toml`:
//!
//! ```toml
//! [owner]
//! type = "group"   # or "user"
//! id = 1234567
//! ```

use serde::{Deserialize, Serialize};

/// Whether a project is owned by a user account or by a group.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum OwnerType {
    Group,
    User,
}

impl std::fmt::Display for OwnerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OwnerType::Group => write!(f, "group"),
            OwnerType::User => write!(f, "user"),
        }
    }
}

/// Who owns the project. Tools without their own owner field fall back to
/// this one.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct Owner {
    /// `"user"` or `"group"`.
    #[serde(rename = "type")]
    pub kind: OwnerType,
    /// The user or group id.
    pub id: u64,
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.kind, self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_type_serializes_lowercase() {
        let s = toml::to_string(&OwnerWrap {
            kind: OwnerType::Group,
        })
        .unwrap();
        assert!(s.contains("kind = \"group\""), "got {}", s);

        let s = toml::to_string(&OwnerWrap {
            kind: OwnerType::User,
        })
        .unwrap();
        assert!(s.contains("kind = \"user\""), "got {}", s);
    }

    #[test]
    fn owner_round_trips_through_toml() {
        let o = Owner {
            kind: OwnerType::Group,
            id: 1234567,
        };
        let s = toml::to_string(&o).unwrap();
        // `type` is the on-disk key thanks to `#[serde(rename = "type")]`.
        assert!(s.contains("type = \"group\""), "got {}", s);
        assert!(s.contains("id = 1234567"), "got {}", s);
        let parsed: Owner = toml::from_str(&s).unwrap();
        assert_eq!(parsed, o);
    }

    #[test]
    fn owner_parses_user_kind() {
        let parsed: Owner = toml::from_str("type = \"user\"\nid = 42\n").unwrap();
        assert_eq!(parsed.kind, OwnerType::User);
        assert_eq!(parsed.id, 42);
    }

    #[test]
    fn owner_type_round_trip_for_both_kinds() {
        for kind in [OwnerType::Group, OwnerType::User] {
            let s = serde_json::to_string(&kind).unwrap();
            let back: OwnerType = serde_json::from_str(&s).unwrap();
            assert_eq!(back, kind);
        }
    }

    /// Helper just to round-trip `OwnerType` standalone via TOML (which only
    /// supports tables at the top level).
    #[derive(Serialize)]
    struct OwnerWrap {
        kind: OwnerType,
    }
}
