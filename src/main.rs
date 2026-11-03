mod app;
mod config;
mod highlighter;
mod history;
mod preferences;
mod print;
mod quick_open;
mod renderer;
mod search_order;
mod vault;
mod window;

use app::ManuscriptApp;

fn main() {
    tracing_subscriber::fmt::init();

    let app = ManuscriptApp::new();
    app.run();
}
