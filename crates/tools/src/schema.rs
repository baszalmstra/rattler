//! Schema generation for rattler_conda_types.
//!
//! This module generates JSON schemas for types in rattler_conda_types that implement
//! `schemars::JsonSchema`. The generated schemas are stored in the `schemas/` directory
//! at the repository root.
//!
//! Where possible, schemas reference the official conda schemas at
//! <https://schemas.conda.org/>.

use crate::{project_root, Mode};
use schemars::JsonSchema;
use std::fs;
use std::path::PathBuf;

/// Base URL for official conda schemas.
const CONDA_SCHEMAS_BASE: &str = "https://schemas.conda.org";

/// Returns the path to the schemas directory.
fn schemas_dir() -> PathBuf {
    project_root().join("schemas")
}

/// Generate a root schema for a type.
fn generate_root_schema<T: JsonSchema>() -> schemars::schema::RootSchema {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.option_nullable = false;
        s.option_add_null_type = false;
    });
    let gen = settings.into_generator();
    gen.into_root_schema_for::<T>()
}

/// Map a type name to its official conda schema reference, if available.
fn official_schema_ref(type_name: &str) -> Option<String> {
    match type_name {
        "PackageName" => Some(format!(
            "{CONDA_SCHEMAS_BASE}/common-1.schema.json#/definitions/name"
        )),
        "Version" => Some(format!(
            "{CONDA_SCHEMAS_BASE}/common-1.schema.json#/definitions/package_version"
        )),
        "TimestampMs" => Some(format!(
            "{CONDA_SCHEMAS_BASE}/common-1.schema.json#/definitions/timestamp"
        )),
        "Md5Hash" => Some(format!(
            "{CONDA_SCHEMAS_BASE}/repodata-record-1.schema.json#/properties/md5"
        )),
        "Sha256Hash" => Some(format!(
            "{CONDA_SCHEMAS_BASE}/repodata-record-1.schema.json#/properties/sha256"
        )),
        _ => None,
    }
}

/// Convert internal `#/definitions/` references to external references.
/// Uses official conda schema URLs where available, otherwise uses local files.
fn externalize_refs(schema: &mut schemars::schema::RootSchema) {
    fn update_refs(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(ref_str)) = map.get("$ref") {
                    if let Some(type_name) = ref_str.strip_prefix("#/definitions/") {
                        let new_ref = official_schema_ref(type_name)
                            .unwrap_or_else(|| format!("{type_name}.json"));
                        map.insert("$ref".to_string(), serde_json::Value::String(new_ref));
                    }
                }
                for v in map.values_mut() {
                    update_refs(v);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    update_refs(v);
                }
            }
            _ => {}
        }
    }

    let mut value = serde_json::to_value(&*schema).expect("schema serialization failed");
    update_refs(&mut value);

    if let serde_json::Value::Object(ref mut map) = value {
        map.remove("definitions");
    }

    *schema = serde_json::from_value(value).expect("schema deserialization failed");
}

/// Update or verify a schema file.
fn update_schema_file(name: &str, contents: &str, mode: Mode) -> anyhow::Result<()> {
    let path = schemas_dir().join(format!("{name}.json"));

    match mode {
        Mode::Overwrite => {
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
fn generate_and_save_schema<T: JsonSchema>(
    name: &str,
    mode: Mode,
    externalize: bool,
) -> anyhow::Result<()> {
    let mut schema = generate_root_schema::<T>();
    if externalize {
        externalize_refs(&mut schema);
    }
    let contents =
        serde_json::to_string_pretty(&schema).expect("failed to serialize schema") + "\n";
    update_schema_file(name, &contents, mode)
}

/// Generate or verify all JSON schemas.
pub fn generate(mode: Mode) -> anyhow::Result<()> {
    use rattler_conda_types::{
        package::RunExportsJson, Arch, NoArchType, PackageRecord, Platform,
    };

    let mut errors = Vec::new();

    // Types that don't have official conda schema equivalents
    let standalone: &[(&str, fn(&str, Mode, bool) -> anyhow::Result<()>)] = &[
        ("Platform", generate_and_save_schema::<Platform>),
        ("Arch", generate_and_save_schema::<Arch>),
        ("NoArchType", generate_and_save_schema::<NoArchType>),
        ("RunExportsJson", generate_and_save_schema::<RunExportsJson>),
    ];

    for (name, gen_fn) in standalone {
        if let Err(e) = gen_fn(name, mode, false) {
            errors.push((*name, e));
        }
    }

    // Composite types (reference official schemas where available)
    let composite: &[(&str, fn(&str, Mode, bool) -> anyhow::Result<()>)] =
        &[("PackageRecord", generate_and_save_schema::<PackageRecord>)];

    for (name, gen_fn) in composite {
        if let Err(e) = gen_fn(name, mode, true) {
            errors.push((*name, e));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        for (name, e) in &errors {
            eprintln!("Error generating schema for {name}: {e}");
        }
        anyhow::bail!("{} schema(s) failed", errors.len());
    }
}
