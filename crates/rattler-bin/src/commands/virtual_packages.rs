use rattler_conda_types::GenericVirtualPackage;
use rattler_virtual_packages::VirtualPackageOverrides;

#[derive(Debug, clap::Parser)]
pub struct Opt {}

pub fn virtual_packages(_opt: Opt) -> anyhow::Result<()> {
    let virtual_packages =
        rattler_virtual_packages::VirtualPackage::detect(&VirtualPackageOverrides::default())?;
    for package in virtual_packages {
        println!("{}", GenericVirtualPackage::from(package.clone()));
    }
    Ok(())
}
