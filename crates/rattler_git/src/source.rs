/// Derived from `uv-git` implementation
/// Source: <https://github.com/astral-sh/uv/blob/main/crates/uv-git/src/source.rs>
/// This module expose `GitSource` type that represents a remote Git source that
/// can be checked out locally.
use std::{
    borrow::Cow,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};

use rattler_networking::LazyClient;
use tracing::instrument;

use crate::{
    GitError, GitUrl, Reporter,
    credentials::GIT_STORE,
    git::GitRemote,
    resolver::RepositoryReference,
    sha::{GitOid, GitSha},
    url::RepositoryUrl,
};

/// A remote Git source that can be checked out locally.
pub struct GitSource {
    /// The Git reference from the manifest file.
    git: GitUrl,
    /// The HTTP client to use for fetching.
    client: LazyClient,
    /// The path to the Git source database.
    cache: PathBuf,
    /// The reporter to use for this source.
    reporter: Option<Arc<dyn Reporter>>,
}

impl GitSource {
    /// Initialize a new Git source.
    pub fn new(git: GitUrl, client: impl Into<LazyClient>, cache: impl Into<PathBuf>) -> Self {
        Self {
            git,
            client: client.into(),
            cache: cache.into(),
            reporter: None,
        }
    }

    /// Set the [`Reporter`] to use for the [`GitSource`].
    #[must_use]
    pub fn with_reporter(self, reporter: Arc<dyn Reporter>) -> Self {
        Self {
            reporter: Some(reporter),
            ..self
        }
    }

    /// Fetch the underlying Git repository at the given revision.
    #[instrument(skip(self), fields(repository = %self.git.repository(), rev = self.git.precise().map(tracing::field::display)))]
    pub fn fetch(self) -> Result<Fetch, GitError> {
        let lfs_requested = self.git.lfs().enabled();

        // Compute the canonical URL for the repository.
        let canonical = RepositoryUrl::new(self.git.repository());

        // The path to the repo, within the Git database. The bare DB itself
        // is shared across LFS / submodule preferences: LFS objects may be
        // present in the DB without affecting other consumers, and
        // submodules are never materialised in the bare DB.
        let ident = cache_digest(&canonical);
        let db_path = self.cache.join("db").join(&ident);

        // Authenticate the URL, if necessary.
        let remote = if let Some(credentials) = GIT_STORE.get(&canonical) {
            Cow::Owned(credentials.apply(self.git.repository().clone()))
        } else {
            Cow::Borrowed(self.git.repository())
        };

        let remote = GitRemote::new(&remote);

        // Try to open the existing database, logging a warning if it's corrupted
        let existing_db = match remote.db_at(&db_path) {
            Ok(db) => Some(db),
            Err(GitError::InvalidRepository(path)) => {
                tracing::warn!(
                    "Detected corrupted git cache at {} (not a valid git repository), removing and re-cloning",
                    path.display()
                );
                None
            }
            Err(_) => None,
        };

        let (db, actual_rev, task) = match (self.git.precise(), existing_db) {
            // If we have a locked revision, and we have a preexisting database
            // which has that revision, then no update needs to happen, but
            // only if LFS artifacts are also present when LFS was requested.
            (Some(rev), Some(db))
                if db.contains(rev.into())
                    && (!lfs_requested || db.contains_lfs_artifacts(rev.into())) =>
            {
                tracing::debug!(
                    "Using existing Git source `{}` pointed at `{}`",
                    self.git.repository(),
                    rev
                );
                let db = db.with_lfs_ready(lfs_requested.then_some(true));
                (db, rev, None)
            }

            // ... otherwise we use this state to update the git database. Note
            // that we still check for being offline here, for example in the
            // situation that we have a locked revision but the database
            // doesn't have it.
            (locked_rev, db) => {
                tracing::debug!("Updating Git source `{}`", self.git.repository());

                // Report the checkout operation to the reporter.
                let task = self.reporter.as_ref().map(|reporter| {
                    reporter.on_checkout_start(remote.url(), self.git.reference().as_rev())
                });

                let (db, actual_rev) = remote.checkout(
                    &db_path,
                    db,
                    self.git.reference(),
                    locked_rev.map(GitOid::from),
                    self.git.lfs(),
                    &self.client,
                )?;

                (db, GitSha::from(actual_rev), task)
            }
        };

        // Don’t use the full hash, in order to contribute less to reaching the
        // path length limit on Windows.
        let short_id = db.to_short_id(actual_rev.into())?;

        // Namespace the checkout dir by the full `GitUrl` when LFS is enabled
        // so that LFS-enabled and LFS-disabled checkouts of the same revision
        // don't trample each other. Mirrors uv's behaviour. We only re-hash
        // when LFS is enabled to keep the path stable for the common case.
        let checkout_ident = if lfs_requested {
            cache_digest_git_url(&self.git)
        } else {
            ident
        };

        // Check out `actual_rev` from the database to a scoped location on the
        // filesystem. This will use hard links and such to ideally make the
        // checkout operation here pretty fast.
        let checkout_path = self
            .cache
            .join("checkouts")
            .join(&checkout_ident)
            .join(short_id.as_str());

        tracing::debug!(
            "Copying git revision `{}` to path `{}`",
            actual_rev,
            checkout_path.display()
        );
        let checkout = db.copy_to(
            actual_rev.into(),
            &checkout_path,
            self.git.repository(),
            self.git.lfs(),
            self.git.submodules(),
        )?;

        // Report the checkout operation to the reporter.
        if let (Some(task), Some(reporter)) = (task, self.reporter.as_ref()) {
            reporter.on_checkout_complete(remote.url(), short_id.as_str(), task);
        }

        tracing::trace!("Finished fetching Git source `{}`", self.git.repository());

        Ok(Fetch {
            repository: RepositoryReference {
                url: canonical,
                reference: self.git.reference().clone(),
            },
            commit: actual_rev,
            path: checkout_path,
            lfs_ready: checkout.lfs_ready().unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Fetch {
    /// The [`RepositoryReference`] reference that was fetched.
    repository: RepositoryReference,

    /// The precise git checkout
    commit: GitSha,

    /// The path to the checked-out repository.
    path: PathBuf,

    /// Whether Git LFS artifacts have been initialized and validated for this
    /// checkout. Always `false` when LFS wasn't requested.
    lfs_ready: bool,
}

impl Fetch {
    pub fn repository(&self) -> &RepositoryReference {
        &self.repository
    }

    pub fn commit(&self) -> GitSha {
        self.commit
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn into_path(self) -> PathBuf {
        self.path
    }

    /// Whether Git LFS artifacts have been fetched and validated for this
    /// checkout. Returns `false` if LFS was not requested for this fetch.
    pub fn lfs_ready(&self) -> bool {
        self.lfs_ready
    }
}

pub fn cache_digest(url: &RepositoryUrl) -> String {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{hash:x}")
}

/// Hash digest for a full `GitUrl`, used to namespace the checkout dir when
/// LFS is enabled (so LFS-vs-non-LFS checkouts of the same revision don't
/// share a directory).
fn cache_digest_git_url(git: &GitUrl) -> String {
    let mut hasher = DefaultHasher::new();
    git.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{hash:x}")
}
