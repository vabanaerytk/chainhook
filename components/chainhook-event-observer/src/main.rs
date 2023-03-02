#[macro_use]
extern crate rocket;

#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate serde_json;

#[macro_use]
extern crate hiro_system_kit;

pub extern crate bitcoincore_rpc;

pub mod chainhooks;
pub mod indexer;
pub mod observer;
pub mod utils;

use crate::utils::Context;
use hiro_system_kit::log::setup_logger;
use hiro_system_kit::slog;
use rocket::config;

use crate::chainhooks::types::ChainhookConfig;
use clap::Parser;
use ctrlc;
use observer::{EventHandler, EventObserverConfig, ObserverCommand};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::sync::mpsc::channel;

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Path of the config to load
    #[clap(short, long)]
    config_path: Option<String>,
}

#[rocket::main]
async fn main() {
    let context = Context {
        logger: Some(setup_logger()),
        tracer: false,
    };

    let args = Args::parse();
    let config_path = get_config_path_or_exit(&args.config_path, &context);
    let config = EventObserverConfig::from_path(&config_path, &context);
    let (command_tx, command_rx) = channel();
    let tx_terminator = command_tx.clone();

    ctrlc::set_handler(move || {
        tx_terminator
            .send(ObserverCommand::Terminate)
            .expect("Could not send signal on channel.")
    })
    .expect("Error setting Ctrl-C handler");

    let _ = observer::start_event_observer(config, command_tx, command_rx, None, context).await;
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct EventObserverConfigFile {
    pub normalization_enabled: Option<bool>,
    pub grpc_server_enabled: Option<bool>,
    pub hooks_enabled: Option<bool>,
    pub bitcoin_rpc_proxy_enabled: Option<bool>,
    pub webhooks: Option<Vec<String>>,
    pub ingestion_port: Option<u16>,
    pub control_port: Option<u16>,
    pub bitcoin_node_username: String,
    pub bitcoin_node_password: String,
    pub bitcoin_node_rpc_url: String,
    pub stacks_node_rpc_url: String,
    pub operators: Option<Vec<String>>,
    pub cache_path: Option<String>,
}

impl EventObserverConfig {
    pub fn from_path(path: &PathBuf, ctx: &Context) -> EventObserverConfig {
        let path = match File::open(path) {
            Ok(path) => path,
            Err(_e) => {
                ctx.try_log(|logger| {
                    slog::error!(
                        logger,
                        "Error: unable to locate Clarinet.toml in current directory"
                    )
                });
                std::process::exit(1);
            }
        };
        let mut file_reader = BufReader::new(path);
        let mut file_buffer = vec![];
        file_reader.read_to_end(&mut file_buffer).unwrap();

        let file: EventObserverConfigFile = match toml::from_slice(&file_buffer[..]) {
            Ok(s) => s,
            Err(e) => {
                ctx.try_log(|logger| error!(logger, "Unable to read config {}", e));
                std::process::exit(1);
            }
        };

        EventObserverConfig::from_config_file(file, ctx)
    }

    pub fn from_config_file(
        mut config_file: EventObserverConfigFile,
        _ctx: &Context,
    ) -> EventObserverConfig {
        let event_handlers = match config_file.webhooks.take() {
            Some(webhooks) => webhooks
                .into_iter()
                .map(|h| EventHandler::WebHook(h))
                .collect::<Vec<_>>(),
            None => vec![],
        };
        let mut operators = HashSet::new();
        if let Some(operator_keys) = config_file.operators.take() {
            for operator_key in operator_keys.into_iter() {
                operators.insert(operator_key);
            }
        }

        let config = EventObserverConfig {
            normalization_enabled: config_file.normalization_enabled.unwrap_or(true),
            grpc_server_enabled: config_file.grpc_server_enabled.unwrap_or(false),
            hooks_enabled: config_file.hooks_enabled.unwrap_or(false),
            chainhook_config: Some(ChainhookConfig::new()),
            bitcoin_rpc_proxy_enabled: config_file.bitcoin_rpc_proxy_enabled.unwrap_or(false),
            event_handlers: event_handlers,
            ingestion_port: config_file
                .ingestion_port
                .unwrap_or(observer::DEFAULT_INGESTION_PORT),
            control_port: config_file
                .control_port
                .unwrap_or(observer::DEFAULT_CONTROL_PORT),
            bitcoin_node_username: config_file.bitcoin_node_username.clone(),
            bitcoin_node_password: config_file.bitcoin_node_password.clone(),
            bitcoin_node_rpc_url: config_file.bitcoin_node_rpc_url.clone(),
            stacks_node_rpc_url: config_file.stacks_node_rpc_url.clone(),
            operators,
            display_logs: true,
            cache_path: config_file.cache_path.unwrap_or("cache".into()),
        };
        config
    }
}

fn get_config_path_or_exit(path: &Option<String>, ctx: &Context) -> PathBuf {
    if let Some(path) = path {
        let manifest_path = PathBuf::from(path);
        if !manifest_path.exists() {
            ctx.try_log(|logger| slog::error!(logger, "Could not find Observer.toml"));
            std::process::exit(1);
        }
        manifest_path
    } else {
        let mut current_dir = std::env::current_dir().unwrap();
        loop {
            current_dir.push("Observer.toml");

            if current_dir.exists() {
                break current_dir;
            }
            current_dir.pop();

            if !current_dir.pop() {
                ctx.try_log(|logger| slog::error!(logger, "Could not find Observer.toml"));
                std::process::exit(1);
            }
        }
    }
}
