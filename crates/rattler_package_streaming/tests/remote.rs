#![cfg(feature = "reqwest")]

//! Tests for the remote extraction pipeline, which downloads into a spooled
//! pipe while extracting synchronously from the other end on a blocking
//! thread.

use std::{path::Path, time::Duration};

use futures_util::StreamExt;
use reqwest::Client;
use reqwest_middleware::ClientWithMiddleware;
use url::Url;

/// Serves `data` at `/<name>`, streaming it in `chunk_size` chunks with
/// `delay` between chunks to simulate a slow download.
async fn serve_bytes(name: &str, data: Vec<u8>, chunk_size: usize, delay: Duration) -> Url {
    use axum::{body::Body, http::header, routing::get};

    let content_length = data.len().to_string();
    let app = axum::Router::new().route(
        &format!("/{name}"),
        get(move || {
            let data = data.clone();
            let content_length = content_length.clone();
            async move {
                let chunks = data
                    .chunks(chunk_size)
                    .map(|chunk| Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(chunk)))
                    .collect::<Vec<_>>();
                let stream = futures_util::stream::iter(chunks).then(move |chunk| async move {
                    tokio::time::sleep(delay).await;
                    chunk
                });
                (
                    [(header::CONTENT_LENGTH, content_length)],
                    Body::from_stream(stream),
                )
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/{name}").parse().unwrap()
}

fn client() -> ClientWithMiddleware {
    ClientWithMiddleware::from(Client::new())
}

const DATA_DESCRIPTOR_PACKAGE: &str = "tests/resources/ca-certificates-2024.7.4-hbcca054_0.conda";
const DATA_DESCRIPTOR_SHA256: &str =
    "6a5d6d8a1a7552dbf8c617312ef951a77d2dac09f2aeaba661deebce603a7a97";
const DATA_DESCRIPTOR_MD5: &str = "a1d1adb5a5dc516dfb3dccc7b9b574a9";

/// Extracts a `.conda` package that downloads slowly. The package uses
/// zip data descriptors, so streaming extraction fails while the download
/// is still in progress and the extractor falls back to re-reading the
/// buffered data from the start — exercising the seek-back path of the
/// spooled pipe concurrently with the download.
// Skip on windows as the test package contains symbolic links
#[cfg_attr(target_os = "windows", ignore)]
#[tokio::test(flavor = "multi_thread")]
async fn test_extract_slow_remote_conda_with_data_descriptors() {
    let data = std::fs::read(DATA_DESCRIPTOR_PACKAGE).unwrap();
    let url = serve_bytes(
        "ca-certificates-2024.7.4-hbcca054_0.conda",
        data,
        16 * 1024,
        Duration::from_millis(10),
    )
    .await;

    let target_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("slow_remote_data_descriptor");
    let result = rattler_package_streaming::reqwest::tokio::extract_conda(
        client(),
        url,
        &target_dir,
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(hex::encode(result.sha256), DATA_DESCRIPTOR_SHA256);
    assert_eq!(hex::encode(result.md5), DATA_DESCRIPTOR_MD5);
}

/// Extracts a `.tar.bz2` package that downloads slowly, in chunks smaller
/// than the bzip2 block size, so the extractor repeatedly blocks waiting
/// for more data.
#[tokio::test(flavor = "multi_thread")]
async fn test_extract_slow_remote_tar_bz2() {
    // Craft a small tar.bz2 archive in memory.
    let mut builder = tar::Builder::new(Vec::new());
    let content = vec![42u8; 256 * 1024];
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(1_700_000_000);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder
        .append_data(&mut header, "blob.bin", content.as_slice())
        .unwrap();
    let tar_data = builder.into_inner().unwrap();

    let mut bz2_data = Vec::new();
    let mut encoder = bzip2::write::BzEncoder::new(&mut bz2_data, bzip2::Compression::fast());
    std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
    encoder.finish().unwrap();

    let expected_sha256 = rattler_digest::compute_bytes_digest::<rattler_digest::Sha256>(&bz2_data);
    let expected_len = bz2_data.len() as u64;
    let url = serve_bytes(
        "slow-package-0.1.0-0.tar.bz2",
        bz2_data,
        4 * 1024,
        Duration::from_millis(5),
    )
    .await;

    let target_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("slow_remote_tar_bz2");
    let result = rattler_package_streaming::reqwest::tokio::extract_tar_bz2(
        client(),
        url,
        &target_dir,
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(result.sha256, expected_sha256);
    assert_eq!(result.total_size, expected_len);
    assert_eq!(std::fs::read(target_dir.join("blob.bin")).unwrap(), content);
}

/// Extracts the same package many times concurrently from a local server,
/// several rounds in a row, mimicking the load pattern of installing an
/// environment.
// Skip on windows as the test package contains symbolic links
#[cfg_attr(target_os = "windows", ignore)]
#[tokio::test(flavor = "multi_thread")]
async fn test_extract_remote_concurrently() {
    let data = std::fs::read(DATA_DESCRIPTOR_PACKAGE).unwrap();
    let url = serve_bytes(
        "ca-certificates-2024.7.4-hbcca054_0.conda",
        data,
        64 * 1024,
        Duration::ZERO,
    )
    .await;

    let temp_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("concurrent_remote");
    for round in 0..3 {
        let extractions = (0..6).map(|task| {
            let url = url.clone();
            let target_dir = temp_dir.join(format!("round{round}-task{task}"));
            async move {
                rattler_package_streaming::reqwest::tokio::extract_conda(
                    client(),
                    url,
                    &target_dir,
                    None,
                    None,
                )
                .await
            }
        });
        for result in futures_util::future::join_all(extractions).await {
            let result = result.unwrap();
            assert_eq!(hex::encode(result.sha256), DATA_DESCRIPTOR_SHA256);
        }
    }
}

/// Regression test for a deadlock observed with rattler-bin's runtime
/// configuration (`max_blocking_threads(num_cores)`): more concurrent
/// extractions than blocking-pool threads, fed by slow downloads, so the
/// pool is saturated with extractors waiting for data. The downloads must
/// be able to complete without the blocking pool, otherwise nothing ever
/// wakes the extractors.
// Skip on windows as the test package contains symbolic links
#[cfg_attr(target_os = "windows", ignore)]
#[test]
fn test_extract_does_not_deadlock_on_a_tiny_blocking_pool() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let all_extractions = async {
        let data = std::fs::read(DATA_DESCRIPTOR_PACKAGE).unwrap();
        let url = serve_bytes(
            "ca-certificates-2024.7.4-hbcca054_0.conda",
            data,
            16 * 1024,
            Duration::from_millis(5),
        )
        .await;

        let temp_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("tiny_blocking_pool");
        let extractions: Vec<_> = (0..6)
            .map(|task| {
                let url = url.clone();
                let target_dir = temp_dir.join(format!("task{task}"));
                tokio::spawn(async move {
                    rattler_package_streaming::reqwest::tokio::extract_conda(
                        client(),
                        url,
                        &target_dir,
                        None,
                        None,
                    )
                    .await
                })
            })
            .collect();
        for extraction in extractions {
            let result = extraction.await.unwrap().unwrap();
            assert_eq!(hex::encode(result.sha256), DATA_DESCRIPTOR_SHA256);
        }
    };

    runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(120), all_extractions).await })
        .expect("extractions deadlocked on a saturated blocking pool");
}

/// `file://` URLs bypass the download pipeline entirely and extract the
/// seekable package straight from disk, including the data-descriptor
/// fallback.
// Skip on windows as the test package contains symbolic links
#[cfg_attr(target_os = "windows", ignore)]
#[tokio::test(flavor = "multi_thread")]
async fn test_extract_file_url() {
    let path = std::fs::canonicalize(DATA_DESCRIPTOR_PACKAGE).unwrap();
    let url = Url::from_file_path(&path).unwrap();

    let target_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("file_url");
    let result =
        rattler_package_streaming::reqwest::tokio::extract(client(), url, &target_dir, None, None)
            .await
            .unwrap();

    assert_eq!(hex::encode(result.sha256), DATA_DESCRIPTOR_SHA256);
    assert_eq!(hex::encode(result.md5), DATA_DESCRIPTOR_MD5);
}

/// Stress-tests the failure mode that motivated decoupling download from
/// extraction: several large real packages extracted concurrently from
/// conda-forge over HTTPS, in multiple rounds. When extraction applied
/// backpressure to the network this made CDN servers reset the HTTP/2
/// streams under load.
///
/// Ignored by default because it downloads several hundred MB.
#[ignore = "network stress test, downloads several hundred MB"]
#[tokio::test(flavor = "multi_thread")]
async fn test_extract_real_packages_concurrently() {
    let packages = [
        (
            "https://conda.anaconda.org/conda-forge/linux-64/python-3.12.0-hab00c5b_0_cpython.conda",
            "5398ebae6a1ccbfd3f76361eac75f3ac071527a8072627c4bf9008c689034f48",
        ),
        (
            "https://conda.anaconda.org/conda-forge/linux-64/libllvm18-18.1.8-h8b73ec9_2.conda",
            "41993f35731d8f24e4f91f9318d6d68a3cfc4b5cf5d54f193fbb3ffd246bf2b7",
        ),
        (
            "https://conda.anaconda.org/conda-forge/linux-64/gcc_impl_linux-64-13.2.0-h338b0a0_5.conda",
            "baab8f8b9af54959735e629cf6d5ec9378166aa4c68ba8dc98dc0a781d548409",
        ),
        (
            "https://conda.anaconda.org/conda-forge/linux-64/rclone-1.74.3-h519d9b9_0.conda",
            "74a49ffb12e8c974519e856e5cb19d2a3c0aa9538847fa616149a4e7576a8106",
        ),
    ];

    let temp_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("concurrent_real");
    for round in 0..3 {
        let extractions = packages.iter().map(|(url, sha256)| {
            let url: Url = url.parse().unwrap();
            let name = url.path_segments().unwrap().next_back().unwrap();
            let target_dir = temp_dir.join(format!("round{round}-{name}"));
            async move {
                let result = rattler_package_streaming::reqwest::tokio::extract_conda(
                    client(),
                    url.clone(),
                    &target_dir,
                    None,
                    None,
                )
                .await?;
                Ok::<_, rattler_package_streaming::ExtractError>((
                    hex::encode(result.sha256),
                    *sha256,
                    url,
                ))
            }
        });
        for result in futures_util::future::join_all(extractions).await {
            let (actual, expected, url) = result.unwrap();
            assert_eq!(actual, expected, "hash mismatch for {url} in round {round}");
        }
    }
}
