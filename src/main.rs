use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use actix_web::{web, App, HttpServer};
use awc::Client;
use lru::LruCache;
use structopt::StructOpt;
use tokio::sync::{Notify, Semaphore};
use tracing::info;

mod accounts;
mod rpc;
mod types;

use accounts::AccountUpdateManager;
use types::{AccountsDb, ProgramAccountsDb};

#[derive(Debug, structopt::StructOpt)]
#[structopt(about = "Solana RPC cache server")]
struct Options {
    #[structopt(
        short = "w",
        long = "websocket-url",
        default_value = "wss://solana-api.projectserum.com",
        help = "validator or cluster PubSub endpoint"
    )]
    ws_url: String,
    #[structopt(
        short = "r",
        long = "rpc-api-url",
        default_value = "https://solana-api.projectserum.com",
        help = "validator or cluster JSON-RPC endpoint"
    )]
    rpc_url: String,
    #[structopt(
        short = "l",
        long = "listen",
        default_value = "127.0.0.1:8080",
        help = "cache server bind address"
    )]
    addr: String,
    #[structopt(
        short = "p",
        long = "program-request-limit",
        default_value = "5",
        help = "maximum number of concurrent getProgramAccounts cache-to-validator requests"
    )]
    program_accounts_request_limit: usize,
    #[structopt(
        short = "a",
        long = "account-request-limit",
        default_value = "100",
        help = "maximum number of concurrent getAccountInfo cache-to-validator requests"
    )]
    account_info_request_limit: usize,
    #[structopt(
        short = "b",
        long = "body-cache-size",
        default_value = "100",
        help = "maximum response cache size"
    )]
    body_cache_size: usize,
}

#[actix_web::main]
async fn main() {
    let options = Options::from_args();

    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    info!("options: {:?}", options);

    run(options).await;
}

async fn run(options: Options) {
    let accounts = AccountsDb::new();
    let program_accounts = ProgramAccountsDb::new();

    let addr =
        AccountUpdateManager::init(accounts.clone(), program_accounts.clone(), &options.ws_url);

    let rpc_url = options.rpc_url;
    let notify = Arc::new(Notify::new());
    let connection_limit =
        options.account_info_request_limit + options.program_accounts_request_limit;
    let account_info_request_limit = Arc::new(Semaphore::new(options.account_info_request_limit));
    let program_accounts_request_limit =
        Arc::new(Semaphore::new(options.program_accounts_request_limit));
    let body_cache_size = options.body_cache_size;

    HttpServer::new(move || {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .connector(
                awc::Connector::new()
                    .max_http_version(awc::http::Version::HTTP_11)
                    .limit(connection_limit)
                    //.conn_keep_alive(Duration::from_secs(0))
                    //.conn_lifetime(Duration::from_secs(0))
                    .finish(),
            )
            .finish();
        let state = rpc::State {
            accounts: accounts.clone(),
            program_accounts: program_accounts.clone(),
            client,
            tx: addr.clone(),
            rpc_url: rpc_url.clone(),
            map_updated: notify.clone(),
            account_info_request_limit: account_info_request_limit.clone(),
            program_accounts_request_limit: program_accounts_request_limit.clone(),
            lru: RefCell::new(LruCache::new(body_cache_size)),
        };
        App::new()
            .data(state)
            .service(web::resource("/").route(web::post().to(rpc::rpc_handler)))
            .service(web::resource("/metrics").route(web::get().to(rpc::metrics_handler)))
    })
    .bind(options.addr)
    .unwrap()
    .run()
    .await
    .unwrap();
}
