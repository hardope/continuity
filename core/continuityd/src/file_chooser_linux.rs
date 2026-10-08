//! The file picker on Linux, for Send File… in the tray menu and Send
//! Files… in the settings window.
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
use std::path::PathBuf;

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
    dialog.set_modal(parent.is_some());
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
