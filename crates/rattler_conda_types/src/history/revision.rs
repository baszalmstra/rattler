//! Types for representing conda history revisions and package changes.

use std::fmt;
use chrono::{DateTime, Utc};

use crate::{Channel, MatchSpec, PackageName, Version};

/// Install operation specification
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstallOperation {
    /// The packages to install
    pub specs: Vec<MatchSpec>,
}

/// Remove operation specification
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoveOperation {
    /// The package names to remove
    pub names: Vec<PackageName>,
}

/// Update operation specification
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpdateOperation {
    /// The packages to update (empty means update all)
    pub specs: Vec<MatchSpec>,
}

/// Create environment operation specification
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CreateOperation {
    /// The packages to install in the new environment
    pub specs: Vec<MatchSpec>,
}

/// Custom operation specification
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CustomOperation {
    /// Description of the custom operation
    pub description: String,
}

/// Represents a user request that triggered a conda operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UserRequest {
    /// Install specific packages
    Install(InstallOperation),
    /// Remove specific packages  
    Remove(RemoveOperation),
    /// Update specific packages
    Update(UpdateOperation),
    /// Create new environment with specific packages
    Create(CreateOperation),
    /// Custom operation (for tools other than conda)
    Custom(CustomOperation),
}

impl From<InstallOperation> for UserRequest {
    fn from(op: InstallOperation) -> Self {
        UserRequest::Install(op)
    }
}

impl From<RemoveOperation> for UserRequest {
    fn from(op: RemoveOperation) -> Self {
        UserRequest::Remove(op)
    }
}

impl From<UpdateOperation> for UserRequest {
    fn from(op: UpdateOperation) -> Self {
        UserRequest::Update(op)
    }
}

impl From<CreateOperation> for UserRequest {
    fn from(op: CreateOperation) -> Self {
        UserRequest::Create(op)
    }
}

impl From<CustomOperation> for UserRequest {
    fn from(op: CustomOperation) -> Self {
        UserRequest::Custom(op)
    }
}

/// Represents a package change in a revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageChange {
    /// The package name
    pub name: PackageName,
    /// The package version
    pub version: Version,
    /// The channel the package came from
    pub channel: Channel,
    /// The build string
    pub build: Option<String>,
    /// Whether this is an addition (+) or removal (-)
    pub operation: PackageOperation,
}

/// The operation performed on a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PackageOperation {
    /// Package was added
    Add,
    /// Package was removed
    Remove,
}

/// Represents a single revision in conda history.
///
/// Each revision corresponds to a conda operation (install, remove, update, etc.)
/// and contains the timestamp, user request, and list of package changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    /// When this revision occurred
    pub timestamp: DateTime<Utc>,
    /// The user request that triggered this revision
    pub user_request: UserRequest,
    /// The package changes in this revision
    pub diff: Vec<PackageChange>,
    /// The conda command that was run (optional)
    pub command: Option<String>,
    /// The version of the tool that created this revision (optional)
    pub tool_version: Option<String>,
}

impl Revision {
    /// Create a new revision
    pub fn new(timestamp: DateTime<Utc>, user_request: UserRequest, diff: Vec<PackageChange>) -> Self {
        Self {
            timestamp,
            user_request,
            diff,
            command: None,
            tool_version: None,
        }
    }
    
    /// Set the command that was run for this revision
    pub fn with_command(mut self, command: String) -> Self {
        self.command = Some(command);
        self
    }
    
    /// Set the tool version for this revision
    pub fn with_tool_version(mut self, tool_version: String) -> Self {
        self.tool_version = Some(tool_version);
        self
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: Implement conda history format serialization
        write!(f, "==> {} <==\n", self.timestamp.format("%Y-%m-%d %H:%M:%S"))?;
        
        if let Some(cmd) = &self.command {
            writeln!(f, "# cmd: {}", cmd)?;
        }
        
        if let Some(version) = &self.tool_version {
            writeln!(f, "# conda version: {}", version)?;
        }
        
        for change in &self.diff {
            let op = match change.operation {
                PackageOperation::Add => "+",
                PackageOperation::Remove => "-",
            };
            writeln!(f, "{}{}", op, change.name.as_normalized())?;
        }
        
        // Write the user request specs
        match &self.user_request {
            UserRequest::Install(op) => {
                let specs_str = op.specs.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ");
                writeln!(f, "# install specs: [{}]", specs_str)?;
            },
            UserRequest::Remove(op) => {
                let names_str = op.names.iter().map(|n| n.as_source()).collect::<Vec<_>>().join(", ");
                writeln!(f, "# remove specs: [{}]", names_str)?;
            },
            UserRequest::Update(op) => {
                let specs_str = if op.specs.is_empty() {
                    "--all".to_string()
                } else {
                    op.specs.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ")
                };
                writeln!(f, "# update specs: [{}]", specs_str)?;
            },
            UserRequest::Create(op) => {
                let specs_str = op.specs.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ");
                writeln!(f, "# create specs: [{}]", specs_str)?;
            },
            UserRequest::Custom(op) => {
                writeln!(f, "# custom specs: [{}]", op.description)?;
            },
        }
        writeln!(f)?;
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::str::FromStr;

    #[test]
    fn test_user_request_variants() {
        let install_op = InstallOperation { specs: vec![] };
        let remove_op = RemoveOperation { names: vec![] };
        let custom_op = CustomOperation { description: "pip".to_string() };
        
        assert_eq!(UserRequest::Install(install_op.clone()), UserRequest::Install(install_op));
        assert_eq!(UserRequest::Custom(custom_op.clone()), UserRequest::Custom(custom_op));
        assert_ne!(UserRequest::Install(InstallOperation { specs: vec![] }), UserRequest::Remove(remove_op));
    }
    
    #[test]
    fn test_package_operation() {
        assert_ne!(PackageOperation::Add, PackageOperation::Remove);
    }
    
    #[test]
    fn test_package_change_construction() {
        let name = PackageName::new_unchecked("numpy");
        let version = Version::from_str("1.21.0").unwrap();
        let channel = Channel::from_str("conda-forge", &crate::channel::ChannelConfig::default_with_root_dir(std::env::current_dir().unwrap())).unwrap();
        
        let change = PackageChange {
            name: name.clone(),
            version: version.clone(),
            channel: channel.clone(),
            build: Some("py38_0".to_string()),
            operation: PackageOperation::Add,
        };
        
        assert_eq!(change.name, name);
        assert_eq!(change.version, version);
        assert_eq!(change.operation, PackageOperation::Add);
        assert_eq!(change.build, Some("py38_0".to_string()));
    }
    
    #[test]
    fn test_revision_construction() {
        let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
        let user_request: UserRequest = InstallOperation { specs: vec![] }.into();
        let diff = vec![];
        
        let revision = Revision::new(timestamp, user_request.clone(), diff.clone());
        assert_eq!(revision.timestamp, timestamp);
        assert_eq!(revision.user_request, user_request);
        assert_eq!(revision.diff, diff);
        assert_eq!(revision.command, None);
        assert_eq!(revision.tool_version, None);
    }
    
    #[test]
    fn test_revision_with_builder_methods() {
        let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
        let user_request: UserRequest = UpdateOperation { specs: vec![] }.into();
        let diff = vec![];
        let command = "conda update numpy".to_string();
        let tool_version = "22.11.1".to_string();
        
        let revision = Revision::new(timestamp, user_request.clone(), diff.clone())
            .with_command(command.clone())
            .with_tool_version(tool_version.clone());
        
        assert_eq!(revision.command, Some(command));
        assert_eq!(revision.tool_version, Some(tool_version));
        assert_eq!(revision.timestamp, timestamp);
        assert_eq!(revision.user_request, user_request);
        assert_eq!(revision.diff, diff);
    }
    
    #[test]
    fn test_revision_with_individual_methods() {
        let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
        let revision_base = Revision::new(timestamp, InstallOperation { specs: vec![] }.into(), vec![]);
        
        // Test with_command only
        let with_cmd = revision_base.clone().with_command("conda install numpy".to_string());
        assert_eq!(with_cmd.command, Some("conda install numpy".to_string()));
        assert_eq!(with_cmd.tool_version, None);
        
        // Test with_tool_version only  
        let with_version = revision_base.clone().with_tool_version("22.11.1".to_string());
        assert_eq!(with_version.command, None);
        assert_eq!(with_version.tool_version, Some("22.11.1".to_string()));
    }
    
    #[test]
    fn test_revision_display_basic() {
        let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
        let revision = Revision::new(timestamp, CreateOperation { specs: vec![] }.into(), vec![]);
        
        let display = revision.to_string();
        assert!(display.contains("==> 2023-01-01 12:00:00 <=="));
        assert!(display.contains("create specs"));
    }
    
    #[test]
    fn test_operation_from_conversions() {
        // Test From implementations
        let install_op = InstallOperation { specs: vec![] };
        let user_request: UserRequest = install_op.into();
        assert!(matches!(user_request, UserRequest::Install(_)));
        
        let remove_op = RemoveOperation { names: vec![] };
        let user_request: UserRequest = remove_op.into();
        assert!(matches!(user_request, UserRequest::Remove(_)));
        
        let update_op = UpdateOperation { specs: vec![] };
        let user_request: UserRequest = update_op.into();
        assert!(matches!(user_request, UserRequest::Update(_)));
        
        let create_op = CreateOperation { specs: vec![] };
        let user_request: UserRequest = create_op.into();
        assert!(matches!(user_request, UserRequest::Create(_)));
        
        let custom_op = CustomOperation { description: "test".to_string() };
        let user_request: UserRequest = custom_op.into();
        assert!(matches!(user_request, UserRequest::Custom(_)));
    }
}