use serde::Serialize;
use sha2::{Digest, Sha256};

use super::types::{ComponentCategory, ContentRef};

/// Default number of hex characters for short display of content hashes.
pub const SHORT_HASH_LEN: usize = 8;

/// Return the first `SHORT_HASH_LEN` hex characters of a full hash for display.
pub fn short_hash(full_hash: &str) -> &str {
    &full_hash[..full_hash.len().min(SHORT_HASH_LEN)]
}

/// Compute the SHA-256 content hash for a RoleComponent.
/// Hashed fields: description, category, content.
pub fn content_hash_component(
    description: &str,
    category: &ComponentCategory,
    content: &ContentRef,
) -> String {
    #[derive(Serialize)]
    struct Input<'a> {
        description: &'a str,
        category: &'a ComponentCategory,
        content: &'a ContentRef,
    }
    let input = Input {
        description,
        category,
        content,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}

/// Compute the SHA-256 content hash for a DesiredOutcome.
/// Hashed fields: description, success_criteria.
pub fn content_hash_outcome(description: &str, success_criteria: &[String]) -> String {
    #[derive(Serialize)]
    struct Input<'a> {
        description: &'a str,
        success_criteria: &'a [String],
    }
    let input = Input {
        description,
        success_criteria,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}

/// Compute the SHA-256 content hash for a TradeoffConfig (formerly Motivation).
/// Hashed fields: description, acceptable_tradeoffs, unacceptable_tradeoffs.
pub fn content_hash_tradeoff(
    acceptable_tradeoffs: &[String],
    unacceptable_tradeoffs: &[String],
    description: &str,
) -> String {
    #[derive(Serialize)]
    struct Input<'a> {
        acceptable_tradeoffs: &'a [String],
        unacceptable_tradeoffs: &'a [String],
        description: &'a str,
    }
    let input = Input {
        acceptable_tradeoffs,
        unacceptable_tradeoffs,
        description,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}

/// Compute the SHA-256 content hash for a Role composition.
/// Hashed fields: sorted component_ids, outcome_id.
pub fn content_hash_role(component_ids: &[String], outcome_id: &str) -> String {
    #[derive(Serialize)]
    struct Input<'a> {
        component_ids: Vec<&'a str>,
        outcome_id: &'a str,
    }
    let mut sorted: Vec<&str> = component_ids.iter().map(|s| s.as_str()).collect();
    sorted.sort();
    let input = Input {
        component_ids: sorted,
        outcome_id,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}

/// Compute the SHA-256 content hash for an Agent composition.
/// Hashed fields: role_id, tradeoff_id.
pub fn content_hash_agent(role_id: &str, tradeoff_id: &str) -> String {
    #[derive(Serialize)]
    struct Input<'a> {
        role_id: &'a str,
        #[serde(rename = "motivation_id")]
        tradeoff_id: &'a str,
    }
    let input = Input {
        role_id,
        tradeoff_id,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}
