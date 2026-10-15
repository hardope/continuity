//! The file and folder pickers on Linux, for Send File… in the tray menu,
//! and Send Files… and the received-files folder's Change… in the
//! Continuity window.
//!
//! rfd's GTK backend can't be used for this. It runs every dialog on a GTK
//! thread of its own and blocks the calling thread until the dialog closes.
//! In this process GTK already belongs to the main thread — tao's event
//! loop, which is also the thread that asks — so the two end up waiting on
//! each other and the app stops responding (reported for the settings
//! window's Send Files…, and the tray's Send File… went the same way). GTK's
//! own chooser, shown from the main thread without waiting for it, doesn't
//! have that problem: the choice arrives later, as a callback on the same
//! thread.

use gtk::prelude::*;
use std::path::{Path, PathBuf};

/// Shows the chooser and returns at once. `on_chosen` runs later, on the
/// main thread, and only if something was chosen.
pub fn choose_files(parent: Option<&gtk::Window>, title: &str, multiple: bool, on_chosen: impl Fn(Vec<PathBuf>) + 'static) {
    let dialog = gtk::FileChooserDialog::with_buttons(
        Some(title),
        parent,
        gtk::FileChooserAction::Open,
        &[("_Cancel", gtk::ResponseType::Cancel), ("_Send", gtk::ResponseType::Accept)],
    );
    dialog.set_select_multiple(multiple);
    present(dialog, parent.is_some(), on_chosen);
}

/// The same, for one folder, starting in `current` when it exists.
pub fn choose_folder(parent: Option<&gtk::Window>, title: &str, current: Option<&Path>, on_chosen: impl Fn(PathBuf) + 'static) {
    let dialog = gtk::FileChooserDialog::with_buttons(
        Some(title),
        parent,
        gtk::FileChooserAction::SelectFolder,
        &[("_Cancel", gtk::ResponseType::Cancel), ("_Select", gtk::ResponseType::Accept)],
    );
    dialog.set_create_folders(true);
    if let Some(current) = current.filter(|dir| dir.is_dir()) {
        let _ = dialog.set_current_folder(current);
    }
    present(dialog, parent.is_some(), move |mut paths| {
        if let Some(folder) = paths.pop() {
            on_chosen(folder);
        }
    });
}

fn present(dialog: gtk::FileChooserDialog, modal: bool, on_chosen: impl Fn(Vec<PathBuf>) + 'static) {
    dialog.set_modal(modal);
    dialog.connect_response(move |dialog, response| {
        let paths = if response == gtk::ResponseType::Accept { dialog.filenames() } else { Vec::new() };
        // A dialog is a toplevel, which GTK keeps until it's destroyed.
        unsafe { dialog.destroy() };
        if !paths.is_empty() {
            on_chosen(paths);
        }
    });
    dialog.present();
}
