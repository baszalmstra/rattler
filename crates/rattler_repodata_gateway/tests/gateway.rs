use itertools::Itertools;
use rattler_conda_types::{sparse_index::SparseIndex, Channel, ChannelConfig, Platform, RepoData};
use rattler_networking::{AuthenticatedClient, AuthenticationStorage};
use rattler_repodata_gateway::sparse_index::Gateway;
use reqwest::Client;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Instant,
};
use url::Url;

fn conda_json_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-data/channels/conda-forge/linux-64/repodata.json")
}

fn conda_json_path_noarch() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-data/channels/conda-forge/noarch/repodata.json")
}

/// Returns the path to a sparse index of conda forge data.
fn sparse_index_path() -> &'static Path {
    static SPARSE_INDEX_PATH: OnceLock<PathBuf> = OnceLock::new();
    SPARSE_INDEX_PATH.get_or_init(|| {
        let index_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("conda-forge");
        if !index_path.is_dir() {
            println!("No existing sparse index found. Creating one from the conda-forge data in this repository.");

            // Create sparse index from repodata
            let linux_64 = SparseIndex::from(RepoData::from_path(conda_json_path()).unwrap());
            let noarch = SparseIndex::from(RepoData::from_path(conda_json_path_noarch()).unwrap());

            // Write to disk
            linux_64
                .write_index_to(&index_path.join("linux-64"), 1)
                .unwrap();
            noarch.write_index_to(&index_path.join("noarch"), 1).unwrap();

            println!("Sparse index written to: {}", index_path.display());
        } else {
            println!("Reusing existing sparse index at: {}", index_path.display());
        }
        index_path
    })
}

#[tokio::test]
async fn test_gateway() {
    let sparse_index = sparse_index_path();
    let cache_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gateway-cache");

    let before_parse = Instant::now();

    // Create a gateway from the sparse index
    let channel = Channel::from_url(
        Url::from_directory_path(sparse_index).unwrap(),
        None,
        &ChannelConfig::default(),
    );

    let client = AuthenticatedClient::from_client(
        Client::builder()
            .connection_verbose(true)
            .http2_prior_knowledge()
            .build()
            .unwrap(),
        AuthenticationStorage::new("rattler", &PathBuf::from("~/.rattler")),
    );

    let gateway = Gateway::new(client, cache_dir);
    let records = gateway
        .find_recursive_records(
            [&channel],
            vec![Platform::Linux64, Platform::NoArch],
            ["python", "pytorch", "rubin-env"],
        )
        .await
        .unwrap();

    let after_parse = Instant::now();

    println!(
        "Parsing records took {}",
        human_duration::human_duration(&(after_parse - before_parse))
    );

    println!(
        "Number of returned records {}",
        records.values().map(|records| records.len()).sum::<usize>()
    );

    insta::assert_yaml_snapshot!(records
        .into_values()
        .flat_map(|record| record.into_iter())
        .map(|record| format!("{}/{}", &record.package_record.subdir, &record.file_name))
        .sorted()
        .collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
// #[tracing_test::traced_test]
async fn test_remote_gateway() {
    // Create a gateway from the sparse index
    // let repodata_server = test_utils::SimpleChannelServer::new(sparse_index_path());
    // let channel = Channel::from_url(repodata_server.url(), None, &ChannelConfig::default());
    let channel = Channel::from_url(
        Url::parse("https://repo.prefiks.dev/conda-forge/").unwrap(),
        None,
        &ChannelConfig::default(),
    );

    let client = AuthenticatedClient::from_client(
        Client::builder()
            // .http2_prior_knowledge()
            .http2_adaptive_window(true)
            .brotli(true)
            .build()
            .unwrap(),
        AuthenticationStorage::new("rattler", &PathBuf::from("~/.rattler")),
    );

    let cache_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gateway-cache");
    let gateway = Gateway::new(client, &cache_dir);

    // Get all the interesting records
    let before_parse = Instant::now();
    let records = gateway
        .find_recursive_records(
            [&channel],
            vec![Platform::Linux64, Platform::NoArch],
            ["rubin-env"],
        )
        .await
        .unwrap();
    let after_parse = Instant::now();

    println!(
        "Parsing records took {}",
        human_duration::human_duration(&(after_parse - before_parse))
    );

    println!(
        "Number of returned records {}",
        records.values().map(|records| records.len()).sum::<usize>()
    );

    let num_records = records
        .into_values()
        .flat_map(|record| record.into_iter())
        .count();
    assert_eq!(num_records, 337232);
}
