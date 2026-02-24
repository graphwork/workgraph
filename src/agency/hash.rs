use serde::Serialize;
use sha2::{Digest, Sha256};

use super::types::SkillRef;

/// Default number of hex characters for short display of content hashes.
pub const SHORT_HASH_LEN: usize = 8;

/// Return the first `SHORT_HASH_LEN` hex characters of a full hash for display.
pub fn short_hash(full_hash: &str) -> &str {
    &full_hash[..full_hash.len().min(SHORT_HASH_LEN)]
}

/// Compute the SHA-256 content hash for a role based on its immutable fields:
/// skills + desired_outcome + description (canonical YAML).
///
/// Performance, lineage, name, and id are excluded because they are mutable.
pub fn content_hash_role(skills: &[SkillRef], desired_outcome: &str, description: &str) -> String {
    #[derive(Serialize)]
    struct RoleHashInput<'a> {
        skills: &'a [SkillRef],
        desired_outcome: &'a str,
        description: &'a str,
    }
    let input = RoleHashInput {
        skills,
        desired_outcome,
        description,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}

/// Compute the SHA-256 content hash for a motivation based on its immutable fields:
/// acceptable_tradeoffs + unacceptable_tradeoffs + description (canonical YAML).
///
/// Performance, lineage, name, and id are excluded because they are mutable.
pub fn content_hash_motivation(
    acceptable_tradeoffs: &[String],
    unacceptable_tradeoffs: &[String],
    description: &str,
) -> String {
    #[derive(Serialize)]
    struct MotivationHashInput<'a> {
        acceptable_tradeoffs: &'a [String],
        unacceptable_tradeoffs: &'a [String],
        description: &'a str,
    }
    let input = MotivationHashInput {
        acceptable_tradeoffs,
        unacceptable_tradeoffs,
        description,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}

/// Compute the SHA-256 content hash for an agent based on its constituent IDs:
/// role_id + motivation_id.
///
/// This is deterministic: the same (role_id, motivation_id) pair always produces the same agent ID.
pub fn content_hash_agent(role_id: &str, motivation_id: &str) -> String {
    #[derive(Serialize)]
    struct AgentHashInput<'a> {
        role_id: &'a str,
        motivation_id: &'a str,
    }
    let input = AgentHashInput {
        role_id,
        motivation_id,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}
