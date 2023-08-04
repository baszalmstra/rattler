use crate::sparse_index::GatewayError;
use rattler_conda_types::{Channel, Platform};

mod local;
mod remote;

pub enum SubdirSource {
    LocalSparseIndex(local::LocalSparseIndex),
    RemoteSparseIndex(remote::RemoteSparseIndex),
}

impl SubdirSource {
    pub async fn new(channel: Channel, platform: Platform) -> Result<Self, GatewayError> {
        // Determine the type of source of the channel based on the URL scheme.
        let platform_url = channel.platform_url(platform);
        let channel_name = channel.canonical_name().into();

        // If the URL uses the file scheme use that
        if platform_url.scheme() == "file" {
            if let Ok(root) = platform_url.to_file_path() {
                return Ok(SubdirSource::LocalSparseIndex(local::LocalSparseIndex {
                    root,
                    channel_name,
                }));
            }
        }

        // Http based scheme?
        if platform_url.scheme() == "http" || platform_url.scheme() == "https" {
            // SubdirSource::RemoteSparseIndex(
            //     RemoteSparseIndex::new(client, cache_dir, channel, platform).await?,
            // )
            unreachable!()
        }

        Err(GatewayError::InvalidSubdirUrl(platform_url))
    }
}
