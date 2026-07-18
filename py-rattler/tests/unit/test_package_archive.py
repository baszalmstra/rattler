import os

import pytest

from rattler.package_streaming import PackageArchive


@pytest.fixture
def conda_package(test_data_dir: str) -> str:
    return os.path.join(test_data_dir, "clobber/clobber-fd-1-0.1.0-h4616a5c_0.conda")


@pytest.fixture
def tar_bz2_package(test_data_dir: str) -> str:
    return os.path.join(test_data_dir, "clobber/clobber-1-0.1.0-h4616a5c_0.tar.bz2")


@pytest.mark.asyncio
async def test_read_files(conda_package: str) -> None:
    archive = await PackageArchive.from_path(conda_package)
    assert archive.access == "local"

    files = await archive.read_files(["info/index.json", "clobber", "missing"])
    assert files["clobber"] == b"clobber-fd-1\n"
    assert files["info/index.json"] is not None
    assert files["missing"] is None

    assert await archive.read_file("clobber") == b"clobber-fd-1\n"
    assert await archive.read_file("missing") is None


@pytest.mark.asyncio
async def test_typed_metadata(conda_package: str) -> None:
    archive = await PackageArchive.from_path(conda_package)
    index = await archive.index_json()
    assert index.name is not None and index.name.normalized == "clobber-fd-1"
    about = await archive.about_json()
    assert about is not None


@pytest.mark.asyncio
async def test_stream(conda_package: str) -> None:
    archive = await PackageArchive.from_path(conda_package)

    names = [entry.name async for entry in archive.stream("info")]
    assert "info/index.json" in names

    async for entry in archive.stream("pkg"):
        if entry.name == "clobber":
            assert await entry.read() == b"clobber-fd-1\n"
            break


@pytest.mark.asyncio
async def test_tar_bz2(tar_bz2_package: str) -> None:
    archive = await PackageArchive.from_path(tar_bz2_package)

    files = await archive.read_files(["info/index.json", "clobber.txt"])
    assert files["info/index.json"] is not None
    assert files["clobber.txt"] is not None

    names = [entry.name async for entry in archive.stream("pkg")]
    assert "clobber.txt" in names
    assert not any(name.startswith("info/") for name in names)
