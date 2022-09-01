mod render;
mod templates;
mod utils;
mod webserver;

use crate::webserver::{init_database, Database};
use std::env;
use std::net::{SocketAddr, ToSocketAddrs};
use templates::AllTemplates;

const WEB_ROOT: &str = ".";
const CONFIG_DIR: &str = "sqlpage";
const TEMPLATES_DIR: &str = "sqlpage/templates";
const MIGRATIONS_DIR: &str = "sqlpage/migrations";

#[cfg(not(feature = "lambda-web"))]
const DEFAULT_DATABASE: &str = "sqlite://site.db?mode=rwc";
#[cfg(feature = "lambda-web")]
const DEFAULT_DATABASE: &str = "sqlite://:memory:";

pub struct AppState {
    db: Database,
    all_templates: AllTemplates,
}

pub struct Config {
    listen_on: std::net::SocketAddr,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    init_logging();

    // Connect to the database
    let database_url = get_database_url();

    let db = init_database(&database_url).await;

    if let Err(e) = webserver::apply_migrations(&db).await {
        log::error!(
            "An error occurred while running the database migration.
        The path '{MIGRATIONS_DIR}' has to point to a directory, which contains valid SQL files
        with names using the format '<VERSION>_<DESCRIPTION>.sql',
        where <VERSION> is a positive number, and <DESCRIPTION> is a string.
        The current state of migrations will be stored in a table called _sqlx_migrations.\n {e:?}"
        )
    }

    log::info!("Connected to database: {database_url}");
    let listen_on = get_listen_on();
    log::info!("Starting server on {}", listen_on);
    let all_templates = AllTemplates::init();
    let state = AppState { db, all_templates };
    let config = Config { listen_on };
    webserver::http::run_server(config, state).await
}

fn get_listen_on() -> SocketAddr {
    let host_str = env::var("LISTEN_ON").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let mut host_addr = host_str
        .to_socket_addrs()
        .expect("Invalid hostname")
        .next()
        .expect("No hostname");
    if let Ok(port) = env::var("PORT") {
        host_addr.set_port(port.parse().expect("Invalid PORT"));
    }
    host_addr
}

fn init_logging() {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
}

fn get_database_url() -> String {
    env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE.to_string())
}
