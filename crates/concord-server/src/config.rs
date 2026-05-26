use std::env;
use std::net::SocketAddr;

pub struct Config {
    pub database_url: String,
    pub addr: SocketAddr,
    pub max_connections: u32,
}

impl Config {
    pub fn from_env() -> Self {
        let database_url =
            env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let port: u16 = env::var("PORT")
            .unwrap_or_else(|_| "8080".into())
            .parse()
            .expect("PORT must be a valid u16");

        let addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .expect("HOST:PORT must form a valid socket address");

        let max_connections: u32 = env::var("MAX_CONNECTIONS")
            .unwrap_or_else(|_| "10".into())
            .parse()
            .expect("MAX_CONNECTIONS must be a valid u32");

        Self { database_url, addr, max_connections }
    }
}
