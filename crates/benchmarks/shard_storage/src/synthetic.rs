use anyhow::Result;
use rattler_conda_types::{PackageRecord, Shard, ShardedRepodata, ShardedSubdirInfo};
use rattler_digest::{compute_bytes_digest, Sha256, Sha256Hash};
use std::collections::HashMap;

/// Generate synthetic test data for benchmarking
pub fn generate_synthetic_data(
    num_shards: usize,
    packages_per_shard: usize,
) -> Result<(ShardedRepodata, HashMap<Sha256Hash, Shard>)> {
    let mut shards_map = HashMap::new();
    let mut shard_hashes: HashMap<String, Sha256Hash, ahash::RandomState> = HashMap::default();

    println!("Generating {} synthetic shards...", num_shards);

    for shard_idx in 0..num_shards {
        let package_name = format!("test-package-{}", shard_idx);

        // Create a shard with minimal package records
        let mut shard = Shard {
            packages: Default::default(),
            conda_packages: Default::default(),
            removed: Default::default(),
        };

        // Add minimal .tar.bz2 packages (simpler than .conda)
        for pkg_idx in 0..packages_per_shard {
            let version = format!("1.{}.{}", shard_idx, pkg_idx);
            let filename = format!("{}-{}-py39_0.tar.bz2", package_name, version);

            let record = create_minimal_package_record(&package_name, &version, &filename);
            shard.packages.insert(filename.clone(), record);
        }

        // Compute hash of the shard
        let shard_bytes = rmp_serde::to_vec(&shard)?;
        let hash = compute_bytes_digest::<Sha256>(&shard_bytes);

        shard_hashes.insert(package_name.clone(), hash);
        shards_map.insert(hash, shard);

        if (shard_idx + 1) % 100 == 0 {
            println!("  Generated {}/{} shards", shard_idx + 1, num_shards);
        }
    }

    let index = ShardedRepodata {
        info: ShardedSubdirInfo {
            subdir: "linux-64".to_string(),
            base_url: "../../packages/".to_string(),
            shards_base_url: "shards/".to_string(),
            created_at: None,
        },
        shards: shard_hashes,
    };

    println!("Generated {} shards successfully", shards_map.len());

    Ok((index, shards_map))
}

fn create_minimal_package_record(name: &str, version: &str, _filename: &str) -> PackageRecord {
    // Create minimal valid PackageRecord by using JSON deserialization
    // This ensures serialization round-trip compatibility
    let json_str = format!(
        r#"{{
            "name": "{}",
            "version": "{}",
            "build": "py39_0",
            "build_number": 0,
            "subdir": "linux-64",
            "depends": [],
            "size": 100000
        }}"#,
        name, version
    );

    serde_json::from_str(&json_str).expect("valid package record JSON")
}
