// SPDX-License-Identifier: GPL-3.0-only

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> cosmic::iced::Result {
    // Debug helper: print the DNS state the applet sees, without starting the UI.
    if std::env::args().any(|a| a == "--status") {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        match rt.block_on(cosmic_applet_dns::backend::status()) {
            Ok(s) => println!("{s:#?}"),
            Err(e) => println!("error: {e}"),
        }
        return Ok(());
    }

    tracing_subscriber::fmt::init();
    let _ = tracing_log::LogTracer::init();

    tracing::info!("Starting CosmicDNS applet v{VERSION}");

    cosmic_applet_dns::run()
}
