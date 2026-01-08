//! Schema generation for rattler_conda_types.
//!
//! This module generates JSON schemas for types in rattler_conda_types that implement
//! `schemars::JsonSchema`. The generated schemas are stored in the `schemas/` directory
//! at the repository root.

use crate::{project_root, Mode};
use schemars::{schema_for, JsonSchema};
use std::fs;
use std::path::PathBuf;

/// Returns the path to the schemas directory.
fn schemas_dir() -> PathBuf {
    project_root().join("schemas")
}

/// Generate a JSON schema for a type and return it as a pretty-printed string.
fn generate_schema<T: JsonSchema>() -> String {
    let schema = schema_for!(T);
    serde_json::to_string_pretty(&schema).expect("failed to serialize schema")
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

/// A macro to generate schemas for multiple types.
macro_rules! generate_schemas {
    ($mode:expr, $( $type:ty => $name:expr ),* $(,)?) => {{
        let mut errors = Vec::new();
        $(
            let schema = generate_schema::<$type>();
            if let Err(e) = update_schema_file($name, &schema, $mode) {
                errors.push(e);
            }
        )*
        if errors.is_empty() {
            Ok(())
        } else {
            for e in &errors {
                eprintln!("Error: {}", e);
            }
            anyhow::bail!("{} schema(s) failed verification", errors.len());
        }
    }};
}

/// Generate or verify all JSON schemas.
pub fn generate(mode: Mode) -> anyhow::Result<()> {
    use rattler_conda_types::{Arch, Platform};

    generate_schemas!(
        mode,
        Platform => "Platform",
        Arch => "Arch",
    )
}
