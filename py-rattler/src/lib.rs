mod channel;
mod error;
mod generic_virtual_package;
mod match_spec;
mod nameless_match_spec;
mod networking;
mod package_name;
mod platform;
mod prefix_record;
mod repo_data;
mod shell;
mod version;
mod virtual_package;

use channel::{PyChannel, PyChannelConfig};
use error::{
    ActivationException, InvalidChannelException, InvalidMatchSpecException,
    InvalidPackageNameException, InvalidUrlException, InvalidVersionException, ParseArchException,
    ParsePlatformException, PyRattlerError,
};
use generic_virtual_package::PyGenericVirtualPackage;
use match_spec::PyMatchSpec;
use nameless_match_spec::PyNamelessMatchSpec;
use networking::PyAuthenticatedClient;
use package_name::PyPackageName;
use prefix_record::{PyPrefixPaths, PyPrefixRecord};
use repo_data::package_record::PyPackageRecord;
use version::PyVersion;

use pyo3::prelude::*;

use platform::{PyArch, PyPlatform};
use shell::{PyActivationResult, PyActivationVariables, PyActivator, PyShellEnum};
use virtual_package::PyVirtualPackage;

#[pymodule]
fn rattler(py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<PyVersion>().unwrap();

    m.add_class::<PyMatchSpec>().unwrap();
    m.add_class::<PyNamelessMatchSpec>().unwrap();

    m.add_class::<PyPackageRecord>().unwrap();
    m.add_class::<PyPackageName>().unwrap();

    m.add_class::<PyChannel>().unwrap();
    m.add_class::<PyChannelConfig>().unwrap();
    m.add_class::<PyPlatform>().unwrap();
    m.add_class::<PyArch>().unwrap();

    m.add_class::<PyAuthenticatedClient>().unwrap();

    // Shell activation things
    m.add_class::<PyActivationVariables>().unwrap();
    m.add_class::<PyActivationResult>().unwrap();
    m.add_class::<PyShellEnum>().unwrap();
    m.add_class::<PyActivator>().unwrap();

    m.add_class::<PyGenericVirtualPackage>().unwrap();
    m.add_class::<PyVirtualPackage>().unwrap();
    m.add_class::<PyPrefixRecord>().unwrap();
    m.add_class::<PyPrefixPaths>().unwrap();

    // Exceptions
    m.add(
        "InvalidVersionError",
        py.get_type::<InvalidVersionException>(),
    )
    .unwrap();
    m.add(
        "InvalidMatchSpecError",
        py.get_type::<InvalidMatchSpecException>(),
    )
    .unwrap();
    m.add(
        "InvalidPackageNameError",
        py.get_type::<InvalidPackageNameException>(),
    )
    .unwrap();
    m.add("InvalidUrlError", py.get_type::<InvalidUrlException>())
        .unwrap();
    m.add(
        "InvalidChannelError",
        py.get_type::<InvalidChannelException>(),
    )
    .unwrap();
    m.add("ActivationError", py.get_type::<ActivationException>())
        .unwrap();
    m.add(
        "ParsePlatformError",
        py.get_type::<ParsePlatformException>(),
    )
    .unwrap();
    m.add("ParseArchError", py.get_type::<ParseArchException>())
        .unwrap();

    Ok(())
}
