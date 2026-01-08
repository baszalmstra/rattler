//! Schema generation for rattler_conda_types.
//!
//! This module generates JSON schemas for types in rattler_conda_types that implement
//! `schemars::JsonSchema`. The generated schemas are stored in the `schemas/` directory
//! at the repository root.
//!
//! Schemas use `$ref` to reference other types, keeping each schema focused on its own
//! type while allowing composition.

use crate::{project_root, Mode};
use schemars::JsonSchema;
use std::fs;
use std::path::PathBuf;

/// Returns the path to the schemas directory.
fn schemas_dir() -> PathBuf {
    project_root().join("schemas")
}

/// Generate a root schema for a type, including all referenced definitions.
fn generate_root_schema<T: JsonSchema>() -> schemars::schema::RootSchema {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.option_nullable = false;
        s.option_add_null_type = false;
    });
    let gen = settings.into_generator();
    gen.into_root_schema_for::<T>()
}

/// Update or verify a schema file.
fn update_schema_file(name: &str, contents: &str, mode: Mode) -> anyhow::Result<()> {
    let path = schemas_dir().join(format!("{name}.json"));

    match mode {
        Mode::Overwrite => {
            // Ensure schemas directory exists
            fs::create_dir_all(schemas_dir())?;
            let old_contents = fs::read_to_string(&path).unwrap_or_default();
            let old_contents = old_contents.replace("\r\n", "\n");
            let contents = contents.replace("\r\n", "\n");
            if old_contents != contents {
                eprintln!("updating {}", path.display());
                fs::write(&path, contents)?;
            }
            Ok(())
        }
        Mode::Verify => {
            let old_contents = fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!(
                    "failed to read schema file '{}': {}. Run `cargo run --bin tools -- gen-schemas` to generate it.",
                    path.display(),
                    e
                )
            })?;
            let old_contents = old_contents.replace("\r\n", "\n");
            let contents = contents.replace("\r\n", "\n");
            if old_contents != contents {
                let changes = difference::Changeset::new(&old_contents, &contents, "\n");
                anyhow::bail!(
                    "==================================================\n\
                     Schema `{}` is not up-to-date\n\
                     ==================================================\n\
                     {}\n\n\
                     Run `cargo run --bin tools -- gen-schemas` to update.",
                    path.display(),
                    changes
                );
            }
            Ok(())
        }
    }
}

/// Generate and save a schema for a single type.
fn generate_and_save_schema<T: JsonSchema>(name: &str, mode: Mode) -> anyhow::Result<()> {
    let schema = generate_root_schema::<T>();
    let contents =
        serde_json::to_string_pretty(&schema).expect("failed to serialize schema") + "\n";
    update_schema_file(name, &contents, mode)
}

/// A macro to generate schemas for multiple types.
macro_rules! generate_schemas {
    ($mode:expr, $( $type:ty => $name:expr ),* $(,)?) => {{
        let mut errors = Vec::new();
        $(
            if let Err(e) = generate_and_save_schema::<$type>($name, $mode) {
                errors.push(($name, e));
            }
        )*
        if errors.is_empty() {
            Ok(())
        } else {
            for (name, e) in &errors {
                eprintln!("Error generating schema for {}: {}", name, e);
            }
            anyhow::bail!("{} schema(s) failed", errors.len());
        }
    }};
}

/// Generate or verify all JSON schemas.
pub fn generate(mode: Mode) -> anyhow::Result<()> {
    use rattler_conda_types::{
        package::RunExportsJson, utils::TimestampMs, Arch, NoArchType, PackageName, PackageRecord,
        Platform, VersionWithSource,
    };
    use rattler_digest::serde::SerializableHash;

    generate_schemas!(
        mode,
        Platform => "Platform",
        Arch => "Arch",
        NoArchType => "NoArchType",
        PackageName => "PackageName",
        VersionWithSource => "Version",
        TimestampMs => "TimestampMs",
        RunExportsJson => "RunExportsJson",
        SerializableHash<rattler_digest::Md5> => "Md5Hash",
        SerializableHash<rattler_digest::Sha256> => "Sha256Hash",
        PackageRecord => "PackageRecord",
    )
}
