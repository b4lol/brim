//! Brim GUI — GTK4/Libadwaita app store frontend over brim-core.

mod css;
mod icons;
mod rows;
mod window;
mod worker;

use adw::prelude::*;
use libadwaita as adw;

fn main() -> gtk4::glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("dev.brim.Store")
        .build();

    // Load the stylesheet once per application run, not per window.
    app.connect_startup(|_| css::load_css());
    app.connect_activate(window::build);

    // Propagate the application's exit code instead of always returning 0.
    app.run()
}
