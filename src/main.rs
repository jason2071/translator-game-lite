// Hide the console window that Windows would otherwise attach to the GUI
// process (release builds only; `cargo run` keeps the console for logs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod core;
mod database;
mod engine;
mod glossary_ai;
mod qa;
mod translation;
mod ui;

fn main() {
    let db = match database::Db::open_default() {
        Ok(db) => std::sync::Arc::new(db),
        Err(e) => {
            eprintln!("failed to open database: {e:#}");
            std::process::exit(1);
        }
    };
    if let Err(e) = ui::run(db) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
