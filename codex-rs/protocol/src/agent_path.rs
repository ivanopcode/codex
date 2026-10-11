use codex_utils_string::take_bytes_at_char_boundary;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;
use ts_rs::TS;

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema, TS,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
#[ts(type = "string")]
pub struct AgentPath(String);

impl AgentPath {
    pub const ROOT: &str = "/root";
    pub const MORPHEUS: &str = "/morpheus";
    const ROOT_SEGMENT: &str = "root";

    /// Maximum length of a single path segment in bytes for NEWLY MINTED
    /// child identities.
    ///
    /// Enforced only by [`AgentPath::join_new`], which the V2 spawn entry
    /// uses for model-supplied task names. Parsing and deserialization
    /// ([`AgentPath::from_string`], `TryFrom`, `FromStr`, serde) stay
    /// grammar-only so previously persisted identities keep working
    /// byte-identically on resume. Longer names are rejected instead of
    /// silently truncated: the path is an identity used for routing.
    pub const MAX_SEGMENT_BYTES: usize = 128;

    /// Maximum length of a full absolute agent path in bytes for NEWLY
    /// MINTED child identities (see [`AgentPath::MAX_SEGMENT_BYTES`]).
    pub const MAX_PATH_BYTES: usize = 1024;

    /// Maximum bytes of an agent path rendered into model-visible display
    /// text before truncation (see [`AgentPath::truncated_for_display`]).
    ///
    /// Legacy over-cap identities can still reach model input through
    /// resume, so the envelope render truncates for display only; the
    /// stored routing identity is never changed.
    pub const MODEL_DISPLAY_MAX_BYTES: usize = 256;

    pub fn root() -> Self {
        Self(Self::ROOT.to_string())
    }

    pub fn morpheus() -> Self {
        Self(Self::MORPHEUS.to_string())
    }

    pub fn from_string(path: String) -> Result<Self, String> {
        validate_absolute_path(path.as_str())?;
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn is_root(&self) -> bool {
        self.as_str() == Self::ROOT
    }

    pub fn name(&self) -> &str {
        if self.is_root() {
            return Self::ROOT_SEGMENT;
        }
        self.as_str()
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .unwrap_or(Self::ROOT_SEGMENT)
    }

    pub fn join(&self, agent_name: &str) -> Result<Self, String> {
        validate_agent_name(agent_name)?;
        Self::from_string(format!("{self}/{agent_name}"))
    }

    /// Joins a NEW child identity, enforcing the new-identity length caps
    /// ([`AgentPath::MAX_SEGMENT_BYTES`], [`AgentPath::MAX_PATH_BYTES`]).
    ///
    /// This is the only constructor that rejects over-long names: the V2
    /// spawn entry mints child paths through here so model-supplied task
    /// names get a clear tool error before any thread exists. An existing
    /// (possibly legacy over-cap) parent keeps its identity; only the new
    /// segment and the resulting total length are bounded.
    pub fn join_new(&self, agent_name: &str) -> Result<Self, String> {
        validate_new_agent_name(agent_name)?;
        let joined = format!("{self}/{agent_name}");
        validate_new_path_length(joined.as_str())?;
        Self::from_string(joined)
    }

    /// Renders this path for model-visible text, truncating it to
    /// [`AgentPath::MODEL_DISPLAY_MAX_BYTES`] bytes with a marker when over
    /// the cap. Paths at or under the cap render byte-identical. Display
    /// only: the stored routing identity is unchanged.
    pub fn truncated_for_display(&self) -> String {
        let rendered = self.as_str();
        if rendered.len() <= Self::MODEL_DISPLAY_MAX_BYTES {
            return rendered.to_string();
        }
        format!(
            "{}{TRUNCATED_DISPLAY_MARKER}",
            take_bytes_at_char_boundary(rendered, Self::MODEL_DISPLAY_MAX_BYTES)
        )
    }

    pub fn resolve(&self, reference: &str) -> Result<Self, String> {
        if reference.is_empty() {
            return Err("agent path must not be empty".to_string());
        }
        if reference == Self::ROOT {
            return Ok(Self::root());
        }
        if reference.starts_with('/') {
            return Self::try_from(reference);
        }

        validate_relative_reference(reference)?;
        Self::from_string(format!("{self}/{reference}"))
    }
}

impl TryFrom<String> for AgentPath {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_string(value)
    }
}

impl TryFrom<&str> for AgentPath {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_string(value.to_string())
    }
}

impl From<AgentPath> for String {
    fn from(value: AgentPath) -> Self {
        value.0
    }
}

impl FromStr for AgentPath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl AsRef<str> for AgentPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for AgentPath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for AgentPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Marker appended when display rendering truncates an agent path.
const TRUNCATED_DISPLAY_MARKER: &str = "…[truncated]";

fn validate_agent_name(agent_name: &str) -> Result<(), String> {
    if agent_name.is_empty() {
        return Err("agent_name must not be empty".to_string());
    }
    if agent_name == AgentPath::ROOT_SEGMENT {
        return Err("agent_name `root` is reserved".to_string());
    }
    if agent_name == "." || agent_name == ".." {
        return Err(format!("agent_name `{agent_name}` is reserved"));
    }
    if agent_name.contains('/') {
        return Err("agent_name must not contain `/`".to_string());
    }
    if !agent_name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err(
            "agent_name must use only lowercase letters, digits, and underscores".to_string(),
        );
    }
    Ok(())
}

fn validate_absolute_path(path: &str) -> Result<(), String> {
    let path_len = path.len();
    if path_len > AgentPath::MAX_PATH_BYTES {
        return Err(format!(
            "absolute agent path must be at most {} bytes, got {path_len}",
            AgentPath::MAX_PATH_BYTES
        ));
    }
    if path == AgentPath::MORPHEUS {
        return Ok(());
    }

    let Some(stripped) = path.strip_prefix('/') else {
        return Err("absolute agent paths must start with `/root` or be `/morpheus`".to_string());
    };
    let mut segments = stripped.split('/');
    let Some(root) = segments.next() else {
        return Err("absolute agent path must not be empty".to_string());
    };
    if root != AgentPath::ROOT_SEGMENT {
        return Err("absolute agent paths must start with `/root` or be `/morpheus`".to_string());
    }
    if stripped.ends_with('/') {
        return Err("absolute agent path must not end with `/`".to_string());
    }
    for segment in segments {
        validate_agent_name(segment)?;
    }
    Ok(())
}

fn validate_relative_reference(reference: &str) -> Result<(), String> {
    if reference.ends_with('/') {
        return Err("relative agent path must not end with `/`".to_string());
    }
    for segment in reference.split('/') {
        validate_agent_name(segment)?;
    }
    Ok(())
}

/// Validates a model-supplied segment for a NEW child identity: the shared
/// grammar plus the new-identity segment cap.
fn validate_new_agent_name(agent_name: &str) -> Result<(), String> {
    let name_len = agent_name.len();
    if name_len > AgentPath::MAX_SEGMENT_BYTES {
        return Err(format!(
            "agent_name must be at most {} bytes, got {name_len}",
            AgentPath::MAX_SEGMENT_BYTES
        ));
    }
    validate_agent_name(agent_name)
}

/// Validates the total length of a NEWLY MINTED absolute path. Only the
/// total is bounded here: an existing legacy parent keeps its identity.
fn validate_new_path_length(path: &str) -> Result<(), String> {
    let path_len = path.len();
    if path_len > AgentPath::MAX_PATH_BYTES {
        return Err(format!(
            "absolute agent path must be at most {} bytes, got {path_len}",
            AgentPath::MAX_PATH_BYTES
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::AgentPath;
    use pretty_assertions::assert_eq;

    #[test]
    fn root_has_expected_name() {
        let root = AgentPath::root();
        assert_eq!(root.as_str(), AgentPath::ROOT);
        assert_eq!(root.name(), "root");
        assert!(root.is_root());
    }

    #[test]
    fn morpheus_has_expected_name() {
        let morpheus = AgentPath::morpheus();
        assert_eq!(morpheus.as_str(), AgentPath::MORPHEUS);
        assert_eq!(morpheus.name(), "morpheus");
        assert!(!morpheus.is_root());
    }

    #[test]
    fn join_builds_child_paths() {
        let root = AgentPath::root();
        let child = root.join("researcher").expect("child path");
        assert_eq!(child.as_str(), "/root/researcher");
        assert_eq!(child.name(), "researcher");
    }

    #[test]
    fn resolve_supports_relative_and_absolute_references() {
        let current = AgentPath::try_from("/root/researcher").expect("path");
        assert_eq!(
            current.resolve("worker").expect("relative path"),
            AgentPath::try_from("/root/researcher/worker").expect("path")
        );
        assert_eq!(
            current.resolve("/root/other").expect("absolute path"),
            AgentPath::try_from("/root/other").expect("path")
        );
    }

    #[test]
    fn invalid_names_and_paths_are_rejected() {
        assert_eq!(
            AgentPath::root().join("BadName"),
            Err("agent_name must use only lowercase letters, digits, and underscores".to_string())
        );
        assert_eq!(
            AgentPath::try_from("/not-root"),
            Err("absolute agent paths must start with `/root` or be `/morpheus`".to_string())
        );
        assert_eq!(
            AgentPath::root().resolve("../sibling"),
            Err("agent_name `..` is reserved".to_string())
        );
    }

    #[test]
    fn agent_path_join_new_rejects_overlong_segment() {
        let overlong = "a".repeat(AgentPath::MAX_SEGMENT_BYTES + 1);
        assert_eq!(
            AgentPath::root().join_new(&overlong),
            Err(format!(
                "agent_name must be at most {} bytes, got {}",
                AgentPath::MAX_SEGMENT_BYTES,
                overlong.len()
            ))
        );
    }

    #[test]
    fn agent_path_join_new_accepts_segment_at_cap() {
        let at_cap = "a".repeat(AgentPath::MAX_SEGMENT_BYTES);
        let child = AgentPath::root()
            .join_new(&at_cap)
            .expect("segment at cap should be accepted");
        assert_eq!(child.as_str(), format!("/root/{at_cap}"));
    }

    #[test]
    fn agent_path_join_new_rejects_path_over_total_cap() {
        // Seven 128-byte segments: "/root" (5) + 7 * 129 == 908. A new
        // 120-byte segment is under the segment cap, but the joined total
        // (908 + 1 + 120 == 1029) exceeds the path cap.
        let segments = vec!["a".repeat(AgentPath::MAX_SEGMENT_BYTES); 7];
        let parent_str = format!("/root/{}", segments.join("/"));
        let parent = AgentPath::try_from(parent_str.as_str()).expect("parent under total cap");
        let segment = "b".repeat(120);
        let joined_len = parent.as_str().len() + 1 + segment.len();
        assert!(joined_len > AgentPath::MAX_PATH_BYTES);
        assert_eq!(
            parent.join_new(&segment),
            Err(format!(
                "absolute agent path must be at most {} bytes, got {joined_len}",
                AgentPath::MAX_PATH_BYTES
            ))
        );
    }

    #[test]
    fn agent_path_accepts_legacy_overlong_segment() {
        // Valid under the pre-cap grammar: parsing, joining and serde
        // round-trip must preserve it byte-identically for resume.
        let legacy = format!("/root/{}", "l".repeat(AgentPath::MAX_SEGMENT_BYTES + 1));
        let parsed = AgentPath::try_from(legacy.as_str()).expect("legacy segment parses");
        assert_eq!(parsed.as_str(), legacy);
        let joined = AgentPath::root()
            .join(&"l".repeat(AgentPath::MAX_SEGMENT_BYTES + 1))
            .expect("legacy segment joins");
        assert_eq!(joined, parsed);
        let round_tripped: AgentPath =
            serde_json::from_value(serde_json::to_value(&parsed).expect("serialize"))
                .expect("legacy segment deserializes");
        assert_eq!(round_tripped, parsed);
    }

    #[test]
    fn agent_path_accepts_legacy_path_over_total_cap() {
        let path = format!("/root/{}", vec!["a"; 600].join("/"));
        assert!(path.len() > AgentPath::MAX_PATH_BYTES);
        let parsed = AgentPath::try_from(path.as_str()).expect("legacy long path parses");
        assert_eq!(parsed.as_str(), path);
        let round_tripped: AgentPath =
            serde_json::from_value(serde_json::to_value(&parsed).expect("serialize"))
                .expect("legacy long path deserializes");
        assert_eq!(round_tripped, parsed);
    }

    #[test]
    fn agent_path_accepts_absolute_path_at_total_cap() {
        // "/root" (5) + 7 segments of 128 (7 * 129) + one segment of 115
        // (116) == 1024.
        let mut segments = vec!["a".repeat(AgentPath::MAX_SEGMENT_BYTES); 7];
        segments.push("b".repeat(115));
        let path = format!("/root/{}", segments.join("/"));
        assert_eq!(path.len(), AgentPath::MAX_PATH_BYTES);
        let parsed = AgentPath::try_from(path.as_str()).expect("path at total cap");
        assert_eq!(parsed.as_str(), path);
    }

    #[test]
    fn agent_path_display_truncates_overlong_path() {
        let short = AgentPath::try_from("/root/worker").expect("short path");
        assert_eq!(short.truncated_for_display(), "/root/worker");
        // Well past the display cap (and the new-spawn segment cap), so the
        // render truncates with a marker. ASCII keeps the slice boundary safe.
        let legacy = format!("/root/{}", "l".repeat(300));
        assert!(legacy.len() > AgentPath::MODEL_DISPLAY_MAX_BYTES);
        let parsed = AgentPath::try_from(legacy.as_str()).expect("legacy path parses");
        let expected = format!(
            "{}…[truncated]",
            &legacy[..AgentPath::MODEL_DISPLAY_MAX_BYTES]
        );
        assert_eq!(parsed.truncated_for_display(), expected);
    }
}
