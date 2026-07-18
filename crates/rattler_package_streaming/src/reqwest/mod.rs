//! Functionality to stream and extract packages directly from a [`reqwest::Url`].
pub mod fetch;
pub mod full_download;
pub mod sparse;
pub mod tokio;

#[cfg(test)]
pub(crate) mod test_server;
