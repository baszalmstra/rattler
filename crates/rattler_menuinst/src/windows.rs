use std::convert::identity;
use std::path::{Path, PathBuf};

use known_folders::KnownFolder;

use crate::{
    render::{BaseMenuItemPlaceholders, MenuItemPlaceholders},
    schema::{Environment, MenuItemCommand, Windows},
    MenuInstError, MenuMode,
};

const SHORTCUT_EXTENSION: &str = "lnk";

// mod knownfolders;
// mod registry;
//
struct Directories {
    programs: PathBuf,
    desktop: Option<PathBuf>,
    quicklaunch: Option<PathBuf>,
}

impl Directories {
    pub fn new(name: &str, mode: MenuMode) -> Result<Self, MenuInstError> {
        let (programs, desktop, quicklaunch) = match mode {
            MenuMode::System => (
                known_folders::get_known_folder_path(KnownFolder::CommonPrograms),
                known_folders::get_known_folder_path(KnownFolder::PublicDesktop),
                None,
            ),
            MenuMode::User => (
                known_folders::get_known_folder_path(KnownFolder::Programs),
                known_folders::get_known_folder_path(KnownFolder::Desktop),
                known_folders::get_known_folder_path(KnownFolder::QuickLaunch),
            ),
        };

        let programs = programs
            .ok_or_else(|| MenuInstError::DirectoryNotFound("program directory".to_string()))?;

        Ok(Directories {
            programs: programs.join(name),
            desktop,
            quicklaunch,
        })
    }
}

pub struct WindowsMenu {
    prefix: PathBuf,
    name: String,
    item: Windows,
    command: MenuItemCommand,
    directories: Directories,
    placeholders: MenuItemPlaceholders,
}

impl WindowsMenu {
    pub fn new(
        prefix: &Path,
        item: Windows,
        command: MenuItemCommand,
        directories: Directories,
        placeholders: &BaseMenuItemPlaceholders,
    ) -> Self {
        let name = command.name.resolve(Environment::Base, placeholders);
        let programs_link_location = directories
            .programs
            .join(&name)
            .with_extension(SHORTCUT_EXTENSION);

        Self {
            prefix: prefix.to_path_buf(),
            name,
            item,
            command,
            directories,
            placeholders: placeholders.refine(&programs_link_location),
        }
    }

    pub fn install(self) -> Result<(), MenuInstError> {
        let paths = [
            Some(&self.directories.programs),
            if self.item.desktop.unwrap_or(false) {
                self.directories.desktop.as_ref()
            } else {
                None
            },
            if self.item.quicklaunch.unwrap_or(false) {
                self.directories.quicklaunch.as_ref()
            } else {
                None
            },
        ];

        for path in paths.into_iter().filter_map(identity) {
            let link_path = path.join(&self.name).with_extension(SHORTCUT_EXTENSION);

            let args = self.build_command_invocation();


        }
    }

    fn build_command_invocation(&self) -> Vec<String> {
        let mut args = Vec::new();
        for cmd in &self.command.command {
            args.push(cmd.resolve(&self.placeholders));
        }
        quote_args(args)
    }
}

fn quote_args(args: impl IntoIterator<Item=String>) -> Vec<String> {
    fn quote(mut arg: String) -> String {
        let unquoted = arg.trim_matches('"');
        if unquoted.starts_with(['-', ' ']) {
            unquoted.to_string()
        } else if unquoted.contains([' ', '/']) {
            format!("\"{unquoted}\"")
        }
        unquoted.to_string()
    }
    args.into_iter().map(quote).collect()
}

pub(crate) fn install_menu_item(
    prefix: &Path,
    menu_name: &str,
    windows_item: Windows,
    command: MenuItemCommand,
    placeholders: &BaseMenuItemPlaceholders,
    menu_mode: MenuMode,
) -> Result<(), MenuInstError> {
    let directories = Directories::new(menu_name, menu_mode)?;

    let menu = WindowsMenu::new(
        prefix,
        menu_name,
        windows_item,
        command,
        directories,
        placeholders,
    );

    // let directories = Directories::new(menu_mode, bundle_name);
    // println!("Installing menu item for {bundle_name}");
    // let menu = crate::macos::MacOSMenu::new(prefix, macos_item, command,
    // directories); menu.install()
    Ok(())
}
