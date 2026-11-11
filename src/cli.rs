use {
    crate::{gui::Ui, portal::Portal},
    clap::Parser,
    error_reporter::Report,
};

/// The xdg-desktop-portal-gtk4 portal.
#[derive(Parser, Debug)]
struct Cli {
    /// Replace the portal if it is already running.
    #[clap(long)]
    pub replace: bool,
}

pub fn main() {
    let args = Cli::parse();
    let ui = Ui::new();
    // GTK has to be initialized (and own the default main context) before the name is
    // taken on the bus: when the portal is started by D-Bus activation, the request that
    // started it arrives at once, and the zbus thread would otherwise run the UI closure
    // itself before gtk::init ("GTK has not been initialized") or hold the main context so
    // that gtk::init fails ("Failed to acquire default main context").
    ui.init_gtk();
    let _portal = match Portal::create(ui.proxy(), args.replace) {
        Ok(p) => p,
        Err(e) => {
            log::error!("Could not create the portal: {}", Report::new(e));
            std::process::exit(1);
        }
    };
    ui.run();
}
