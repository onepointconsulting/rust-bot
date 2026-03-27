mod bus;
mod agent;
mod providers;
mod config;

fn main() {
    config::log::init_logger();
    log::info!("Starting the bot");
}
