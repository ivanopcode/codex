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

    /// Maximum length of a single path segment in bytes.
    ///
    /// Segments are created from model-supplied spawn names and copied verbatim
    /// into model-visible envelope fields (`author`/`recipient`), so longer
    /// names are rejected instead of silently truncated: the path is an
    /// identity used for routing.
    pub const MAX_SEGMENT_BYTES: usize = 128;

    /// Maximum length of a full absolute agent path in bytes.
    ///
    /// Bounds nested spawn-created paths however deep the configured spawn
    /// depth allows, so every `author`/`recipient` pair keeps a fixed upper
    /// bound in model-visible items.
    pub const MAX_PATH_BYTES: usize = 1024;

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

fn validate_agent_name(agent_name: &str) -> Result<(), String> {
    if agent_name.is_empty() {
        return Err("agent_name must not be empty".to_string());
    }
    let name_len = agent_name.len();
    if name_len > AgentPath::MAX_SEGMENT_BYTES {
        return Err(format!(
            "agent_name must be at most {} bytes, got {name_len}",
            AgentPath::MAX_SEGMENT_BYTES
        ));
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
    fn agent_path_rejects_overlong_segment() {
        let overlong = "a".repeat(AgentPath::MAX_SEGMENT_BYTES + 1);
        assert_eq!(
            AgentPath::root().join(&overlong),
            Err(format!(
                "agent_name must be at most {} bytes, got {}",
                AgentPath::MAX_SEGMENT_BYTES,
                overlong.len()
            ))
        );
    }

    #[test]
    fn agent_path_accepts_segment_at_cap() {
        let at_cap = "a".repeat(AgentPath::MAX_SEGMENT_BYTES);
        let child = AgentPath::root()
            .join(&at_cap)
            .expect("segment at cap should be accepted");
        assert_eq!(child.as_str(), format!("/root/{at_cap}"));
    }

    #[test]
    fn agent_path_rejects_absolute_path_over_total_cap() {
        let path = format!("/root/{}", vec!["a"; 600].join("/"));
        assert!(path.len() > AgentPath::MAX_PATH_BYTES);
        assert_eq!(
            AgentPath::try_from(path.as_str()),
            Err(format!(
                "absolute agent path must be at most {} bytes, got {}",
                AgentPath::MAX_PATH_BYTES,
                path.len()
            ))
        );
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
}
