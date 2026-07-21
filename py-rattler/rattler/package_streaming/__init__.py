from __future__ import annotations

from os import PathLike
from typing import AsyncIterator, Callable, Coroutine, Dict, Iterable, List, Literal, Optional, Tuple

from rattler.networking.client import Client
from rattler.package.about_json import AboutJson
from rattler.package.index_json import IndexJson
from rattler.package.paths_json import PathsJson
from rattler.package.run_exports_json import RunExportsJson
from rattler.rattler import PyArchiveEntry, PyPackageArchive, PySectionStream
from rattler.rattler import download_bytes as py_download_bytes
from rattler.rattler import download_to_path as py_download_to_path
from rattler.rattler import download_to_writer as py_download_to_writer
from rattler.rattler import download_and_extract as py_download_and_extract
from rattler.rattler import extract as py_extract
from rattler.rattler import extract_tar_bz2 as py_extract_tar_bz2
from rattler.rattler import fetch_raw_package_file_from_url as py_fetch_raw_package_file_from_url


def extract(path: PathLike[str], dest: PathLike[str]) -> Tuple[bytes, bytes]:
    """Extract a file to a destination."""
    return py_extract(path, dest)


def extract_tar_bz2(path: PathLike[str], dest: PathLike[str]) -> Tuple[bytes, bytes]:
    """Extract a tar.bz2 file to a destination."""
    return py_extract_tar_bz2(path, dest)


async def download_to_path(client: Client, url: str, dest: PathLike[str]) -> None:
    """
    Stream a package archive from a URL to a destination path.

    This method does not buffer the whole response in Python memory. Response
    bytes are fetched incrementally and written directly to `dest`.
    """
    await py_download_to_path(client._client, url, dest)


async def download_bytes(client: Client, url: str) -> bytes:
    """
    Download a package archive from a URL into memory.

    This is a convenience API. The full response body is buffered before the
    `bytes` object is returned, so peak memory use scales with the artifact
    size.
    """
    return await py_download_bytes(client._client, url)


async def download_to_writer(client: Client, url: str, writer: object) -> None:
    """
    Stream a package archive from a URL into a Python writer.

    The response body is fetched incrementally. For each chunk, `writer.write`
    is called with a `bytes` object. The writer must provide a synchronous
    `write(bytes)` method, for example `io.BytesIO()` or an open binary file.
    """
    await py_download_to_writer(client._client, url, writer)


async def download_and_extract(
    client: Client, url: str, dest: PathLike[str], expected_sha: Optional[bytes] = None
) -> Tuple[bytes, bytes]:
    """Download a file from a URL and extract it to a destination."""
    return await py_download_and_extract(client._client, url, dest, expected_sha)


async def fetch_raw_package_file_from_url(client: Client, url: str, path: str) -> bytes:
    """
    Fetch raw bytes for a file inside a remote `.conda` package using sparse
    range requests.

    When reading more than one file from the same package, prefer
    `PackageArchive`, which opens the package once and shares the work
    between reads.
    """
    return await py_fetch_raw_package_file_from_url(client._client, url, path)


class ArchiveEntry:
    """
    One tar entry yielded while streaming a section of a package archive.

    Call `read()` to get the entry contents *before* advancing the stream;
    not calling it skips the entry cheaply.
    """

    def __init__(self, inner: PyArchiveEntry) -> None:
        self._inner = inner

    @property
    def name(self) -> str:
        """The path of the entry inside the package."""
        return self._inner.name

    @property
    def size(self) -> int:
        """The size of the entry contents in bytes."""
        return self._inner.size

    @property
    def is_file(self) -> bool:
        """True if the entry is a regular file (not a directory or link)."""
        return self._inner.is_file

    async def read(self) -> bytes:
        """Reads the contents of this entry."""
        return await self._inner.read()

    def __repr__(self) -> str:
        return f"ArchiveEntry(name={self.name!r}, size={self.size})"


class _SectionStream:
    """Async iterator over the entries of one package section.

    The underlying stream is opened lazily on the first `__anext__` so that
    an iterator that is never consumed does not leave an unawaited coroutine.
    """

    def __init__(self, open_stream: Callable[[], Coroutine[None, None, PySectionStream]]) -> None:
        self._open_stream: Optional[Callable[[], Coroutine[None, None, PySectionStream]]] = open_stream
        self._stream: Optional[PySectionStream] = None

    def __aiter__(self) -> AsyncIterator[ArchiveEntry]:
        return self

    async def __anext__(self) -> ArchiveEntry:
        if self._stream is None:
            if self._open_stream is None:
                raise StopAsyncIteration
            # Consume the opener first so a failed open exhausts the iterator
            # instead of retrying a spent coroutine.
            open_stream, self._open_stream = self._open_stream, None
            self._stream = await open_stream()
        return ArchiveEntry(await self._stream.__anext__())


class PackageArchive:
    """
    A conda package archive (local or remote) that is opened once and can
    then be read many times.

    For remote `.conda` archives on servers that support HTTP range requests,
    opening costs a single range request and reads only download the bytes
    they need. `.tar.bz2` archives and servers without range support
    transparently fall back to downloading the archive once into a temporary
    file.

    Examples
    --------
    ```python
    pkg = await PackageArchive.open(client, url)
    paths = await pkg.paths_json()
    libs = [p.relative_path for p in paths.paths if str(p.relative_path).endswith(".so")]
    files = await pkg.read_files(libs)
    ```
    """

    _inner: PyPackageArchive

    def __init__(self, inner: PyPackageArchive) -> None:
        self._inner = inner

    @staticmethod
    async def open(client: Client, url: str) -> PackageArchive:
        """
        Opens a remote package archive. For `.conda` archives on servers with
        range support this costs a single HTTP range request.
        """
        return PackageArchive(await PyPackageArchive.from_url(client._client, url))

    @staticmethod
    async def from_path(path: PathLike[str] | str) -> PackageArchive:
        """
        Opens a package archive from a local file.

        Examples
        --------
        ```python
        pkg = await PackageArchive.from_path("numpy-2.1.3-py312h58c1407_0.conda")
        index = await pkg.index_json()
        ```
        """
        return PackageArchive(await PyPackageArchive.from_path(path))

    @property
    def access(self) -> Literal["sparse", "local", "spooled"]:
        """How the archive is accessed."""
        return self._inner.access()  # type: ignore[return-value]

    async def read_file(self, path: str) -> Optional[bytes]:
        """
        Reads a single file from the package. Returns `None` if the path does
        not exist in the archive.

        Contents are not cached: every call streams the containing section
        again up to the requested file. When reading more than one file,
        prefer a single `read_files` call.

        Examples
        --------
        ```python
        recipe = await pkg.read_file("info/recipe/meta.yaml")
        if recipe is None:
            print("package has no recipe")
        ```
        """
        return await self._inner.read_file(path)

    async def read_files(self, paths: Iterable[str]) -> Dict[str, Optional[bytes]]:
        """
        Reads multiple files from the package with the minimum amount of
        work: paths are grouped per section and each touched section is
        streamed at most once, aborting as soon as its last requested file
        has been read. The result maps every requested path to its contents,
        or `None` when the path does not exist.

        Calls are independent and may run concurrently, but contents are not
        cached: a repeated call streams its sections again, so batch all
        needed paths into a single call where possible.

        Examples
        --------
        ```python
        # One pass over the payload, one over info, fetched concurrently.
        files = await pkg.read_files(["info/index.json", "lib/libfoo.so", "bin/foo"])
        for path, contents in files.items():
            if contents is None:
                print(f"{path}: not in archive")
        ```
        """
        return await self._inner.read_files(list(paths))

    async def index_json(self) -> IndexJson:
        """Reads and parses `info/index.json`."""
        return IndexJson._from_py_index_json(await self._inner.index_json())

    async def about_json(self) -> AboutJson:
        """Reads and parses `info/about.json`."""
        return AboutJson._from_py_about_json(await self._inner.about_json())

    async def paths_json(self) -> PathsJson:
        """Reads and parses `info/paths.json`."""
        return PathsJson._from_py_paths_json(await self._inner.paths_json())

    async def run_exports_json(self) -> RunExportsJson:
        """Reads and parses `info/run_exports.json`."""
        return RunExportsJson._from_py_run_exports_json(await self._inner.run_exports_json())

    async def list_files(self, section: Literal["info", "pkg"] = "pkg") -> List[str]:
        """
        Lists the paths of all files in one section.

        For `"info"` this is usually served from the cached archive tail. For
        `"pkg"` it streams the entire section; prefer `paths_json()` when only
        paths are needed.

        Examples
        --------
        ```python
        # Usually free: the info section tends to sit in the cached tail.
        for path in await pkg.list_files("info"):
            print(path)
        ```
        """
        return await self._inner.list_files(section)

    def stream(self, section: Literal["info", "pkg"] = "pkg") -> AsyncIterator[ArchiveEntry]:
        """
        Streams the tar entries of one section of the package.

        Every call opens a new independent forward-only iterator (for remote
        archives: a new request). Entries that are not `read()` are skipped
        cheaply, and abandoning the iterator aborts any underlying transfer.

        Examples
        --------
        ```python
        async for entry in pkg.stream("pkg"):
            if entry.name.endswith(".so"):
                data = await entry.read()  # read before advancing
            # entries that are not read are skipped cheaply
        ```
        """
        return _SectionStream(lambda: self._inner.stream(section))

    def __repr__(self) -> str:
        return f"PackageArchive(access={self.access!r})"
