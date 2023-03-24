use crate::chainhooks::bitcoin::{
    evaluate_bitcoin_chainhooks_on_chain_event, handle_bitcoin_hook_action,
    BitcoinChainhookOccurrence, BitcoinChainhookOccurrencePayload,
};
use crate::chainhooks::stacks::{
    evaluate_stacks_chainhooks_on_chain_event, handle_stacks_hook_action,
    StacksChainhookOccurrence, StacksChainhookOccurrencePayload,
};
use crate::chainhooks::types::{
    ChainhookConfig, ChainhookFullSpecification, ChainhookSpecification,
};


use crate::hord::ord::{indexing::updater::OrdinalIndexUpdater, initialize_ordinal_index};
use crate::hord::process_bitcoin_block_using_hord;
use crate::indexer::bitcoin::{
    retrieve_full_block_breakdown_with_retry, standardize_bitcoin_block, BitcoinBlockFullBreakdown,
    NewBitcoinBlock,
};
use crate::indexer::{self, Indexer, IndexerConfig};
use crate::utils::{send_request, Context};

use bitcoincore_rpc::bitcoin::{BlockHash, Txid};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use chainhook_types::{
    BitcoinBlockData, BitcoinChainEvent,
    BitcoinChainUpdatedWithBlocksData, BitcoinChainUpdatedWithReorgData, BitcoinNetwork, BlockIdentifier, BlockchainEvent, StacksChainEvent, StacksNetwork, TransactionIdentifier,
};
use clarity_repl::clarity::util::hash::bytes_to_hex;
use hiro_system_kit;
use hiro_system_kit::slog;
use reqwest::Client as HttpClient;
use rocket::config::{self, Config, LogLevel};
use rocket::data::{Limits, ToByteUnit};
use rocket::http::Status;
use rocket::request::{self, FromRequest, Outcome, Request};
use rocket::serde::json::{json, Json, Value as JsonValue};
use rocket::serde::Deserialize;
use rocket::Shutdown;
use rocket::State;
use rocket_okapi::{openapi, openapi_get_routes, request::OpenApiFromRequest};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::str;
use std::str::FromStr;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub const DEFAULT_INGESTION_PORT: u16 = 20445;
pub const DEFAULT_CONTROL_PORT: u16 = 20446;

#[derive(Deserialize)]
pub struct NewTransaction {
    pub txid: String,
    pub status: String,
    pub raw_result: String,
    pub raw_tx: String,
}

#[derive(Clone, Debug)]
pub enum Event {
    BitcoinChainEvent(BitcoinChainEvent),
    StacksChainEvent(StacksChainEvent),
}

// TODO(lgalabru): Support for GRPC?
#[derive(Clone, Debug)]
pub enum EventHandler {
    WebHook(String),
}

impl EventHandler {
    async fn propagate_stacks_event(&self, stacks_event: &StacksChainEvent) {
        match self {
            EventHandler::WebHook(host) => {
                let path = "chain-events/stacks";
                let url = format!("{}/{}", host, path);
                let body = rocket::serde::json::serde_json::to_vec(&stacks_event).unwrap_or(vec![]);
                let http_client = HttpClient::builder()
                    .timeout(Duration::from_secs(20))
                    .build()
                    .expect("Unable to build http client");
                let _ = http_client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;
                // TODO(lgalabru): handle response errors
            }
        }
    }

    async fn propagate_bitcoin_event(&self, bitcoin_event: &BitcoinChainEvent) {
        match self {
            EventHandler::WebHook(host) => {
                let path = "chain-events/bitcoin";
                let url = format!("{}/{}", host, path);
                let body =
                    rocket::serde::json::serde_json::to_vec(&bitcoin_event).unwrap_or(vec![]);
                let http_client = HttpClient::builder()
                    .timeout(Duration::from_secs(20))
                    .build()
                    .expect("Unable to build http client");
                let _res = http_client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;
                // TODO(lgalabru): handle response errors
            }
        }
    }

    async fn notify_bitcoin_transaction_proxied(&self) {}
}

#[derive(Clone, Debug)]
pub struct EventObserverConfig {
    pub hooks_enabled: bool,
    pub chainhook_config: Option<ChainhookConfig>,
    pub bitcoin_rpc_proxy_enabled: bool,
    pub event_handlers: Vec<EventHandler>,
    pub ingestion_port: u16,
    pub control_port: u16,
    pub bitcoin_node_username: String,
    pub bitcoin_node_password: String,
    pub bitcoin_node_rpc_url: String,
    pub stacks_node_rpc_url: String,
    pub operators: HashSet<String>,
    pub display_logs: bool,
    pub cache_path: String,
    pub bitcoin_network: BitcoinNetwork,
    pub stacks_network: StacksNetwork,
}

impl EventObserverConfig {
    pub fn get_cache_path_buf(&self) -> PathBuf {
        let mut path_buf = PathBuf::new();
        path_buf.push(&self.cache_path);
        path_buf
    }

    pub fn get_bitcoin_config(&self) -> BitcoinConfig {
        let bitcoin_config = BitcoinConfig {
            username: self.bitcoin_node_username.clone(),
            password: self.bitcoin_node_password.clone(),
            rpc_url: self.bitcoin_node_rpc_url.clone(),
        };
        bitcoin_config
    }
}

#[derive(Deserialize, Debug)]
pub struct ContractReadonlyCall {
    pub okay: bool,
    pub result: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObserverCommand {
    ProcessBitcoinBlock(BitcoinBlockFullBreakdown),
    CacheBitcoinBlock(BitcoinBlockData),
    PropagateBitcoinChainEvent(BlockchainEvent),
    PropagateStacksChainEvent(StacksChainEvent),
    PropagateStacksMempoolEvent(StacksChainMempoolEvent),
    RegisterHook(ChainhookFullSpecification, ApiKey),
    DeregisterBitcoinHook(String, ApiKey),
    DeregisterStacksHook(String, ApiKey),
    NotifyBitcoinTransactionProxied,
    Terminate,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StacksChainMempoolEvent {
    TransactionsAdmitted(Vec<MempoolAdmissionData>),
    TransactionDropped(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MempoolAdmissionData {
    pub tx_data: String,
    pub tx_description: String,
}

#[derive(Clone, Debug)]
pub enum ObserverEvent {
    Error(String),
    Fatal(String),
    Info(String),
    BitcoinChainEvent(BitcoinChainEvent),
    StacksChainEvent(StacksChainEvent),
    NotifyBitcoinTransactionProxied,
    HookRegistered(ChainhookSpecification),
    HookDeregistered(ChainhookSpecification),
    BitcoinChainhookTriggered(BitcoinChainhookOccurrencePayload),
    StacksChainhookTriggered(StacksChainhookOccurrencePayload),
    HooksTriggered(usize),
    Terminate,
    StacksChainMempoolEvent(StacksChainMempoolEvent),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
/// JSONRPC Request
pub struct BitcoinRPCRequest {
    /// The name of the RPC call
    pub method: String,
    /// Parameters to the RPC call
    pub params: serde_json::Value,
    /// Identifier for this Request, which should appear in the response
    pub id: serde_json::Value,
    /// jsonrpc field, MUST be "2.0"
    pub jsonrpc: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct BitcoinConfig {
    pub username: String,
    pub password: String,
    pub rpc_url: String,
}

#[derive(Debug, Clone)]
pub struct ServicesConfig {
    pub stacks_node_url: String,
    pub bitcoin_node_url: String,
}

#[derive(Debug, Clone)]
pub struct ChainhookStore {
    entries: HashMap<ApiKey, ChainhookConfig>,
}

impl ChainhookStore {
    pub fn is_authorized(&self, token: Option<String>) -> bool {
        self.entries.contains_key(&ApiKey(token))
    }
}

pub async fn start_event_observer(
    mut config: EventObserverConfig,
    observer_commands_tx: Sender<ObserverCommand>,
    observer_commands_rx: Receiver<ObserverCommand>,
    observer_events_tx: Option<crossbeam_channel::Sender<ObserverEvent>>,
    ctx: Context,
) -> Result<(), Box<dyn Error>> {
    ctx.try_log(|logger| slog::info!(logger, "Event observer starting with config {:?}", config));

    // let ordinal_index = if cfg!(feature = "ordinals") {
    // Start indexer with a receiver in background thread

    ctx.try_log(|logger| {
        slog::info!(
            logger,
            "Initializing ordinals index in dir `{}`",
            config.cache_path
        )
    });

    let ordinal_index = initialize_ordinal_index(&config, None, &ctx)?;
    match OrdinalIndexUpdater::update(&ordinal_index, None, &ctx).await {
        Ok(_r) => {}
        Err(e) => {
            ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
        }
    }

    ctx.try_log(|logger| {
        slog::info!(
            logger,
            "Genesis ordinal indexing successful {:?}",
            ordinal_index.info()
        )
    });

    let indexer_config = IndexerConfig {
        stacks_node_rpc_url: config.stacks_node_rpc_url.clone(),
        bitcoin_node_rpc_url: config.bitcoin_node_rpc_url.clone(),
        bitcoin_node_rpc_username: config.bitcoin_node_username.clone(),
        bitcoin_node_rpc_password: config.bitcoin_node_password.clone(),
        stacks_network: StacksNetwork::Devnet,
        bitcoin_network: BitcoinNetwork::Regtest,
    };

    let indexer = Indexer::new(indexer_config.clone());

    let log_level = if config.display_logs {
        if cfg!(feature = "cli") {
            LogLevel::Critical
        } else {
            LogLevel::Debug
        }
    } else {
        LogLevel::Off
    };

    let ingestion_port = config.ingestion_port;
    let control_port = config.control_port;
    let bitcoin_rpc_proxy_enabled = config.bitcoin_rpc_proxy_enabled;
    let bitcoin_config = config.get_bitcoin_config();

    let services_config = ServicesConfig {
        stacks_node_url: config.bitcoin_node_rpc_url.clone(),
        bitcoin_node_url: config.stacks_node_rpc_url.clone(),
    };

    let mut entries = HashMap::new();
    if config.operators.is_empty() {
        // If authorization not required, we create a default ChainhookConfig
        let mut hook_formation = ChainhookConfig::new();
        if let Some(ref mut initial_chainhook_config) = config.chainhook_config {
            hook_formation
                .stacks_chainhooks
                .append(&mut initial_chainhook_config.stacks_chainhooks);
            hook_formation
                .bitcoin_chainhooks
                .append(&mut initial_chainhook_config.bitcoin_chainhooks);
        }
        entries.insert(ApiKey(None), hook_formation);
    } else {
        for operator in config.operators.iter() {
            entries.insert(ApiKey(Some(operator.clone())), ChainhookConfig::new());
        }
    }
    let chainhook_store = Arc::new(RwLock::new(ChainhookStore { entries }));
    let indexer_rw_lock = Arc::new(RwLock::new(indexer));

    let background_job_tx_mutex = Arc::new(Mutex::new(observer_commands_tx.clone()));

    let limits = Limits::default().limit("json", 4.megabytes());
    let mut shutdown_config = config::Shutdown::default();
    shutdown_config.ctrlc = false;
    shutdown_config.grace = 0;
    shutdown_config.mercy = 0;

    let ingestion_config = Config {
        port: ingestion_port,
        workers: 3,
        address: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        keep_alive: 5,
        temp_dir: std::env::temp_dir().into(),
        log_level: log_level.clone(),
        cli_colors: false,
        limits,
        shutdown: shutdown_config,
        ..Config::default()
    };

    let mut routes = rocket::routes![
        handle_ping,
        handle_new_bitcoin_block,
        handle_new_stacks_block,
        handle_new_microblocks,
        handle_new_mempool_tx,
        handle_drop_mempool_tx,
        handle_new_attachement,
        handle_mined_block,
        handle_mined_microblock,
    ];

    if bitcoin_rpc_proxy_enabled {
        routes.append(&mut routes![handle_bitcoin_rpc_call]);
        routes.append(&mut routes![handle_bitcoin_wallet_rpc_call]);
    }

    let ctx_cloned = ctx.clone();
    let ignite = rocket::custom(ingestion_config)
        .manage(indexer_rw_lock)
        .manage(background_job_tx_mutex)
        .manage(bitcoin_config)
        .manage(ctx_cloned)
        .manage(services_config)
        .mount("/", routes)
        .ignite()
        .await?;
    let ingestion_shutdown = Some(ignite.shutdown());

    let _ = std::thread::spawn(move || {
        let _ = hiro_system_kit::nestable_block_on(ignite.launch());
    });

    let mut shutdown_config = config::Shutdown::default();
    shutdown_config.ctrlc = false;
    shutdown_config.grace = 1;
    shutdown_config.mercy = 1;

    let control_config = Config {
        port: control_port,
        workers: 1,
        address: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        keep_alive: 5,
        temp_dir: std::env::temp_dir().into(),
        log_level,
        cli_colors: false,
        shutdown: shutdown_config,
        ..Config::default()
    };

    let routes = openapi_get_routes![
        handle_ping,
        handle_get_hooks,
        handle_create_hook,
        handle_delete_bitcoin_hook,
        handle_delete_stacks_hook
    ];

    let background_job_tx_mutex = Arc::new(Mutex::new(observer_commands_tx.clone()));
    let managed_chainhook_store = chainhook_store.clone();
    let ctx_cloned = ctx.clone();

    let ignite = rocket::custom(control_config)
        .manage(background_job_tx_mutex)
        .manage(managed_chainhook_store)
        .manage(ctx_cloned)
        .mount("/", routes)
        .ignite()
        .await?;
    let control_shutdown = Some(ignite.shutdown());

    let _ = std::thread::spawn(move || {
        let _ = hiro_system_kit::nestable_block_on(ignite.launch());
    });

    // This loop is used for handling background jobs, emitted by HTTP calls.
    start_observer_commands_handler(
        config,
        chainhook_store,
        observer_commands_rx,
        observer_events_tx,
        ingestion_shutdown,
        control_shutdown,
        ctx,
    )
    .await
}

pub fn get_bitcoin_proof(
    bitcoin_client_rpc: &Client,
    transaction_identifier: &TransactionIdentifier,
    block_identifier: &BlockIdentifier,
) -> Result<String, String> {
    let txid = Txid::from_str(&transaction_identifier.hash[2..]).expect("unable to build txid");
    let block_hash =
        BlockHash::from_str(&block_identifier.hash[2..]).expect("unable to build block_hash");

    let res = bitcoin_client_rpc.get_tx_out_proof(&vec![txid], Some(&block_hash));
    match res {
        Ok(proof) => Ok(format!("0x{}", bytes_to_hex(&proof))),
        Err(e) => Err(format!(
            "failed collecting proof for transaction {}: {}",
            transaction_identifier.hash,
            e.to_string()
        )),
    }
}

pub fn pre_process_bitcoin_block() {}

pub fn apply_bitcoin_block() {}

pub fn rollback_bitcoin_block() {}

pub async fn start_observer_commands_handler(
    config: EventObserverConfig,
    chainhook_store: Arc<RwLock<ChainhookStore>>,
    observer_commands_rx: Receiver<ObserverCommand>,
    observer_events_tx: Option<crossbeam_channel::Sender<ObserverEvent>>,
    ingestion_shutdown: Option<Shutdown>,
    control_shutdown: Option<Shutdown>,
    ctx: Context,
) -> Result<(), Box<dyn Error>> {
    let mut chainhooks_occurrences_tracker: HashMap<String, u64> = HashMap::new();
    let event_handlers = config.event_handlers.clone();
    let mut chainhooks_lookup: HashMap<String, ApiKey> = HashMap::new();
    let networks = (&config.bitcoin_network, &config.stacks_network);
    let mut bitcoin_block_store: HashMap<BlockIdentifier, BitcoinBlockData> = HashMap::new();
    // {
    //     let _ = initialize_hord_db(&config.get_cache_path_buf(), &ctx);
    // }
    loop {
        let command = match observer_commands_rx.recv() {
            Ok(cmd) => cmd,
            Err(e) => {
                if let Some(ref tx) = observer_events_tx {
                    let _ = tx.send(ObserverEvent::Error(format!("Channel error: {:?}", e)));
                }
                continue;
            }
        };
        match command {
            ObserverCommand::Terminate => {
                ctx.try_log(|logger| slog::info!(logger, "Handling Termination command"));
                if let Some(ingestion_shutdown) = ingestion_shutdown {
                    ingestion_shutdown.notify();
                }
                if let Some(control_shutdown) = control_shutdown {
                    control_shutdown.notify();
                }
                if let Some(ref tx) = observer_events_tx {
                    let _ = tx.send(ObserverEvent::Info("Terminating event observer".into()));
                    let _ = tx.send(ObserverEvent::Terminate);
                }
                break;
            }
            ObserverCommand::ProcessBitcoinBlock(block_data) => {
                let mut new_block = standardize_bitcoin_block(&config, block_data, &ctx)?;
                process_bitcoin_block_using_hord(&config, &mut new_block, &ctx);
                bitcoin_block_store.insert(new_block.block_identifier.clone(), new_block);
            }
            ObserverCommand::CacheBitcoinBlock(block) => {
                bitcoin_block_store.insert(block.block_identifier.clone(), block);
            }
            ObserverCommand::PropagateBitcoinChainEvent(blockchain_event) => {
                ctx.try_log(|logger| {
                    slog::info!(logger, "Handling PropagateBitcoinChainEvent command")
                });

                // Update Chain event before propagation
                let chain_event = match blockchain_event {
                    BlockchainEvent::BlockchainUpdatedWithHeaders(data) => {
                        let mut new_blocks = vec![];
                        let mut confirmed_blocks = vec![];

                        for header in data.new_headers.iter() {
                            match bitcoin_block_store.get(&header.block_identifier) {
                                Some(block) => {
                                    new_blocks.push(block.clone());
                                }
                                None => {
                                    ctx.try_log(|logger| {
                                        slog::error!(
                                            logger,
                                            "Unable to retrieve bitcoin block {}",
                                            header.block_identifier
                                        )
                                    });
                                }
                            }
                        }

                        for header in data.confirmed_headers.iter() {
                            match bitcoin_block_store.remove(&header.block_identifier) {
                                Some(block) => {
                                    confirmed_blocks.push(block);
                                }
                                None => {
                                    ctx.try_log(|logger| {
                                        slog::error!(
                                            logger,
                                            "Unable to retrieve confirmed bitcoin block {}",
                                            header.block_identifier
                                        )
                                    });
                                }
                            }
                        }

                        BitcoinChainEvent::ChainUpdatedWithBlocks(
                            BitcoinChainUpdatedWithBlocksData {
                                new_blocks,
                                confirmed_blocks,
                            },
                        )
                    }
                    BlockchainEvent::BlockchainUpdatedWithReorg(data) => {
                        let mut blocks_to_apply = vec![];
                        let mut blocks_to_rollback = vec![];
                        let mut confirmed_blocks = vec![];

                        for header in data.headers_to_apply.iter() {
                            match bitcoin_block_store.get(&header.block_identifier) {
                                Some(block) => {
                                    blocks_to_apply.push(block.clone());
                                }
                                None => {
                                    ctx.try_log(|logger| {
                                        slog::error!(
                                            logger,
                                            "Unable to retrieve bitcoin block {}",
                                            header.block_identifier
                                        )
                                    });
                                }
                            }
                        }

                        for header in data.headers_to_rollback.iter() {
                            match bitcoin_block_store.get(&header.block_identifier) {
                                Some(block) => {
                                    blocks_to_rollback.push(block.clone());
                                }
                                None => {
                                    ctx.try_log(|logger| {
                                        slog::error!(
                                            logger,
                                            "Unable to retrieve bitcoin block {}",
                                            header.block_identifier
                                        )
                                    });
                                }
                            }
                        }

                        for header in data.confirmed_headers.iter() {
                            match bitcoin_block_store.remove(&header.block_identifier) {
                                Some(block) => {
                                    confirmed_blocks.push(block);
                                }
                                None => {
                                    ctx.try_log(|logger| {
                                        slog::error!(
                                            logger,
                                            "Unable to retrieve confirmed bitcoin block {}",
                                            header.block_identifier
                                        )
                                    });
                                }
                            }
                        }

                        BitcoinChainEvent::ChainUpdatedWithReorg(BitcoinChainUpdatedWithReorgData {
                            blocks_to_apply,
                            blocks_to_rollback,
                            confirmed_blocks,
                        })
                    }
                };

                for event_handler in event_handlers.iter() {
                    event_handler.propagate_bitcoin_event(&chain_event).await;
                }
                // process hooks
                let mut hooks_ids_to_deregister = vec![];
                let mut requests = vec![];

                if config.hooks_enabled {
                    match chainhook_store.read() {
                        Err(e) => {
                            ctx.try_log(|logger| {
                                slog::error!(logger, "unable to obtain lock {:?}", e)
                            });
                            continue;
                        }
                        Ok(chainhook_store_reader) => {
                            let bitcoin_chainhooks = chainhook_store_reader
                                .entries
                                .values()
                                .map(|v| &v.bitcoin_chainhooks)
                                .flatten()
                                .collect::<Vec<_>>();
                            ctx.try_log(|logger| {
                                slog::info!(
                                    logger,
                                    "Evaluating {} bitcoin chainhooks registered",
                                    bitcoin_chainhooks.len()
                                )
                            });

                            let chainhooks_candidates = evaluate_bitcoin_chainhooks_on_chain_event(
                                &chain_event,
                                bitcoin_chainhooks,
                                &ctx,
                            );

                            ctx.try_log(|logger| {
                                slog::info!(
                                    logger,
                                    "{} bitcoin chainhooks positive evaluations",
                                    chainhooks_candidates.len()
                                )
                            });

                            let mut chainhooks_to_trigger = vec![];

                            for trigger in chainhooks_candidates.into_iter() {
                                let mut total_occurrences: u64 = *chainhooks_occurrences_tracker
                                    .get(&trigger.chainhook.uuid)
                                    .unwrap_or(&0);
                                total_occurrences += 1;

                                let limit = trigger.chainhook.expire_after_occurrence.unwrap_or(0);
                                if limit == 0 || total_occurrences <= limit {
                                    chainhooks_occurrences_tracker
                                        .insert(trigger.chainhook.uuid.clone(), total_occurrences);
                                    chainhooks_to_trigger.push(trigger);
                                } else {
                                    hooks_ids_to_deregister.push(trigger.chainhook.uuid.clone());
                                }
                            }

                            let bitcoin_client_rpc = Client::new(
                                &config.bitcoin_node_rpc_url,
                                Auth::UserPass(
                                    config.bitcoin_node_username.to_string(),
                                    config.bitcoin_node_password.to_string(),
                                ),
                            )
                            .expect("unable to build http client");

                            let mut proofs = HashMap::new();
                            for hook_to_trigger in chainhooks_to_trigger.iter() {
                                for (transactions, block) in hook_to_trigger.apply.iter() {
                                    for transaction in transactions.iter() {
                                        if !proofs.contains_key(&transaction.transaction_identifier)
                                        {
                                            ctx.try_log(|logger| {
                                                slog::info!(
                                                    logger,
                                                    "collecting proof for transaction {}",
                                                    transaction.transaction_identifier.hash
                                                )
                                            });
                                            match get_bitcoin_proof(
                                                &bitcoin_client_rpc,
                                                &transaction.transaction_identifier,
                                                &block.block_identifier,
                                            ) {
                                                Ok(proof) => {
                                                    proofs.insert(
                                                        &transaction.transaction_identifier,
                                                        proof,
                                                    );
                                                }
                                                Err(e) => {
                                                    ctx.try_log(|logger| {
                                                        slog::error!(logger, "{e}")
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            ctx.try_log(|logger| {
                                slog::info!(
                                    logger,
                                    "{} bitcoin chainhooks will be triggered",
                                    chainhooks_to_trigger.len()
                                )
                            });

                            if let Some(ref tx) = observer_events_tx {
                                let _ = tx.send(ObserverEvent::HooksTriggered(
                                    chainhooks_to_trigger.len(),
                                ));
                            }
                            for chainhook_to_trigger in chainhooks_to_trigger.into_iter() {
                                match handle_bitcoin_hook_action(chainhook_to_trigger, &proofs) {
                                    Err(e) => {
                                        ctx.try_log(|logger| {
                                            slog::error!(logger, "unable to handle action {}", e)
                                        });
                                    }
                                    Ok(BitcoinChainhookOccurrence::Http(request)) => {
                                        requests.push(request);
                                    }
                                    Ok(BitcoinChainhookOccurrence::File(_path, _bytes)) => ctx
                                        .try_log(|logger| {
                                            slog::info!(
                                                logger,
                                                "Writing to disk not supported in server mode"
                                            )
                                        }),
                                    Ok(BitcoinChainhookOccurrence::Data(payload)) => {
                                        if let Some(ref tx) = observer_events_tx {
                                            let _ = tx.send(
                                                ObserverEvent::BitcoinChainhookTriggered(payload),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    };
                }
                ctx.try_log(|logger| {
                    slog::info!(
                        logger,
                        "{} bitcoin chainhooks to deregister",
                        hooks_ids_to_deregister.len()
                    )
                });

                for hook_uuid in hooks_ids_to_deregister.iter() {
                    match chainhook_store.write() {
                        Err(e) => {
                            ctx.try_log(|logger| {
                                slog::error!(logger, "unable to obtain lock {:?}", e)
                            });
                            continue;
                        }
                        Ok(mut chainhook_store_writer) => {
                            chainhooks_lookup
                                .get(hook_uuid)
                                .and_then(|api_key| {
                                    chainhook_store_writer.entries.get_mut(&api_key)
                                })
                                .and_then(|hook_formation| {
                                    hook_formation.deregister_bitcoin_hook(hook_uuid.clone())
                                })
                                .and_then(|chainhook| {
                                    if let Some(ref tx) = observer_events_tx {
                                        let _ = tx.send(ObserverEvent::HookDeregistered(
                                            ChainhookSpecification::Bitcoin(chainhook.clone()),
                                        ));
                                    }
                                    Some(chainhook)
                                });
                        }
                    }
                }

                for request in requests.into_iter() {
                    // todo(lgalabru): collect responses for reporting
                    ctx.try_log(|logger| {
                        slog::info!(
                            logger,
                            "Dispatching request from bitcoin chainhook {:?}",
                            request
                        )
                    });
                    send_request(request, &ctx).await;
                }

                if let Some(ref tx) = observer_events_tx {
                    let _ = tx.send(ObserverEvent::BitcoinChainEvent(chain_event));
                }
            }
            ObserverCommand::PropagateStacksChainEvent(chain_event) => {
                ctx.try_log(|logger| {
                    slog::info!(logger, "Handling PropagateStacksChainEvent command")
                });
                for event_handler in event_handlers.iter() {
                    event_handler.propagate_stacks_event(&chain_event).await;
                }
                let mut hooks_ids_to_deregister = vec![];
                let mut requests = vec![];
                if config.hooks_enabled {
                    match chainhook_store.read() {
                        Err(e) => {
                            ctx.try_log(|logger| {
                                slog::error!(logger, "unable to obtain lock {:?}", e)
                            });
                            continue;
                        }
                        Ok(chainhook_store_reader) => {
                            let stacks_chainhooks = chainhook_store_reader
                                .entries
                                .values()
                                .map(|v| &v.stacks_chainhooks)
                                .flatten()
                                .collect();

                            // process hooks
                            let chainhooks_candidates = evaluate_stacks_chainhooks_on_chain_event(
                                &chain_event,
                                stacks_chainhooks,
                                &ctx,
                            );

                            let mut chainhooks_to_trigger = vec![];

                            for trigger in chainhooks_candidates.into_iter() {
                                let mut total_occurrences: u64 = *chainhooks_occurrences_tracker
                                    .get(&trigger.chainhook.uuid)
                                    .unwrap_or(&0);
                                total_occurrences += 1;

                                let limit = trigger.chainhook.expire_after_occurrence.unwrap_or(0);
                                if limit == 0 || total_occurrences <= limit {
                                    chainhooks_occurrences_tracker
                                        .insert(trigger.chainhook.uuid.clone(), total_occurrences);
                                    chainhooks_to_trigger.push(trigger);
                                } else {
                                    hooks_ids_to_deregister.push(trigger.chainhook.uuid.clone());
                                }
                            }

                            if let Some(ref tx) = observer_events_tx {
                                let _ = tx.send(ObserverEvent::HooksTriggered(
                                    chainhooks_to_trigger.len(),
                                ));
                            }
                            let proofs = HashMap::new();
                            for chainhook_to_trigger in chainhooks_to_trigger.into_iter() {
                                match handle_stacks_hook_action(chainhook_to_trigger, &proofs, &ctx)
                                {
                                    Err(e) => {
                                        ctx.try_log(|logger| {
                                            slog::error!(logger, "unable to handle action {}", e)
                                        });
                                    }
                                    Ok(StacksChainhookOccurrence::Http(request)) => {
                                        requests.push(request);
                                    }
                                    Ok(StacksChainhookOccurrence::File(_path, _bytes)) => ctx
                                        .try_log(|logger| {
                                            slog::info!(
                                                logger,
                                                "Writing to disk not supported in server mode"
                                            )
                                        }),
                                    Ok(StacksChainhookOccurrence::Data(payload)) => {
                                        if let Some(ref tx) = observer_events_tx {
                                            let _ = tx.send(
                                                ObserverEvent::StacksChainhookTriggered(payload),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                for hook_uuid in hooks_ids_to_deregister.iter() {
                    match chainhook_store.write() {
                        Err(e) => {
                            ctx.try_log(|logger| {
                                slog::error!(logger, "unable to obtain lock {:?}", e)
                            });
                            continue;
                        }
                        Ok(mut chainhook_store_writer) => {
                            chainhooks_lookup
                                .get(hook_uuid)
                                .and_then(|api_key| {
                                    chainhook_store_writer.entries.get_mut(&api_key)
                                })
                                .and_then(|hook_formation| {
                                    hook_formation.deregister_stacks_hook(hook_uuid.clone())
                                })
                                .and_then(|chainhook| {
                                    if let Some(ref tx) = observer_events_tx {
                                        let _ = tx.send(ObserverEvent::HookDeregistered(
                                            ChainhookSpecification::Stacks(chainhook.clone()),
                                        ));
                                    }
                                    Some(chainhook)
                                });
                        }
                    }
                }

                for request in requests.into_iter() {
                    // todo(lgalabru): collect responses for reporting
                    ctx.try_log(|logger| {
                        slog::info!(
                            logger,
                            "Dispatching request from stacks chainhook {:?}",
                            request
                        )
                    });
                    send_request(request, &ctx).await;
                }

                if let Some(ref tx) = observer_events_tx {
                    let _ = tx.send(ObserverEvent::StacksChainEvent(chain_event));
                }
            }
            ObserverCommand::PropagateStacksMempoolEvent(mempool_event) => {
                ctx.try_log(|logger| {
                    slog::info!(logger, "Handling PropagateStacksMempoolEvent command")
                });
                if let Some(ref tx) = observer_events_tx {
                    let _ = tx.send(ObserverEvent::StacksChainMempoolEvent(mempool_event));
                }
            }
            ObserverCommand::NotifyBitcoinTransactionProxied => {
                ctx.try_log(|logger| {
                    slog::info!(logger, "Handling NotifyBitcoinTransactionProxied command")
                });
                for event_handler in event_handlers.iter() {
                    event_handler.notify_bitcoin_transaction_proxied().await;
                }
                if let Some(ref tx) = observer_events_tx {
                    let _ = tx.send(ObserverEvent::NotifyBitcoinTransactionProxied);
                }
            }
            ObserverCommand::RegisterHook(hook, api_key) => match chainhook_store.write() {
                Err(e) => {
                    ctx.try_log(|logger| slog::error!(logger, "unable to obtain lock {:?}", e));
                    continue;
                }
                Ok(mut chainhook_store_writer) => {
                    ctx.try_log(|logger| slog::info!(logger, "Handling RegisterHook command"));
                    let hook_formation = match chainhook_store_writer.entries.get_mut(&api_key) {
                        Some(hook_formation) => hook_formation,
                        None => {
                            ctx.try_log(|logger| {
                                slog::error!(
                                    logger,
                                    "Unable to retrieve chainhooks associated with {:?}",
                                    api_key
                                )
                            });
                            continue;
                        }
                    };

                    let spec = match hook_formation.register_hook(networks, hook, &api_key) {
                        Ok(uuid) => uuid,
                        Err(e) => {
                            ctx.try_log(|logger| {
                                slog::error!(
                                    logger,
                                    "Unable to retrieve register new chainhook spec: {}",
                                    e.to_string()
                                )
                            });
                            continue;
                        }
                    };
                    chainhooks_lookup.insert(spec.uuid().to_string(), api_key.clone());
                    ctx.try_log(|logger| {
                        slog::info!(
                            logger,
                            "Registering chainhook {} associated with {:?}",
                            spec.uuid(),
                            api_key
                        )
                    });
                    if let Some(ref tx) = observer_events_tx {
                        let _ = tx.send(ObserverEvent::HookRegistered(spec));
                    }
                }
            },
            ObserverCommand::DeregisterStacksHook(hook_uuid, api_key) => {
                match chainhook_store.write() {
                    Err(e) => {
                        ctx.try_log(|logger| slog::error!(logger, "unable to obtain lock {:?}", e));
                        continue;
                    }
                    Ok(mut chainhook_store_writer) => {
                        ctx.try_log(|logger| {
                            slog::info!(logger, "Handling DeregisterStacksHook command")
                        });
                        let hook_formation = match chainhook_store_writer.entries.get_mut(&api_key)
                        {
                            Some(hook_formation) => hook_formation,
                            None => {
                                ctx.try_log(|logger| {
                                    slog::error!(
                                        logger,
                                        "Unable to retrieve chainhooks associated with {:?}",
                                        api_key
                                    )
                                });
                                continue;
                            }
                        };
                        chainhooks_lookup.remove(&hook_uuid);
                        let hook = hook_formation.deregister_stacks_hook(hook_uuid);
                        if let (Some(tx), Some(hook)) = (&observer_events_tx, hook) {
                            let _ = tx.send(ObserverEvent::HookDeregistered(
                                ChainhookSpecification::Stacks(hook),
                            ));
                        }
                    }
                }
            }
            ObserverCommand::DeregisterBitcoinHook(hook_uuid, api_key) => {
                match chainhook_store.write() {
                    Err(e) => {
                        ctx.try_log(|logger| slog::error!(logger, "unable to obtain lock {:?}", e));
                        continue;
                    }
                    Ok(mut chainhook_store_writer) => {
                        ctx.try_log(|logger| {
                            slog::info!(logger, "Handling DeregisterBitcoinHook command")
                        });
                        let hook_formation = match chainhook_store_writer.entries.get_mut(&api_key)
                        {
                            Some(hook_formation) => hook_formation,
                            None => {
                                ctx.try_log(|logger| {
                                    slog::error!(
                                        logger,
                                        "Unable to retrieve chainhooks associated with {:?}",
                                        api_key
                                    )
                                });
                                continue;
                            }
                        };
                        chainhooks_lookup.remove(&hook_uuid);
                        let hook = hook_formation.deregister_bitcoin_hook(hook_uuid);
                        if let (Some(tx), Some(hook)) = (&observer_events_tx, hook) {
                            let _ = tx.send(ObserverEvent::HookDeregistered(
                                ChainhookSpecification::Bitcoin(hook),
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[openapi(skip)]
#[rocket::get("/ping", format = "application/json")]
pub fn handle_ping(ctx: &State<Context>) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "GET /ping"));
    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(skip)]
#[post("/new_burn_block", format = "json", data = "<bitcoin_block>")]
pub async fn handle_new_bitcoin_block(
    indexer_rw_lock: &State<Arc<RwLock<Indexer>>>,
    bitcoin_config: &State<BitcoinConfig>,
    bitcoin_block: Json<NewBitcoinBlock>,
    background_job_tx: &State<Arc<Mutex<Sender<ObserverCommand>>>>,
    ctx: &State<Context>,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /new_burn_block"));
    // Standardize the structure of the block, and identify the
    // kind of update that this new block would imply, taking
    // into account the last 7 blocks.

    let block_hash = bitcoin_block.burn_block_hash.strip_prefix("0x").unwrap();
    let block =
        match retrieve_full_block_breakdown_with_retry(block_hash, bitcoin_config, ctx).await {
            Ok(block) => block,
            Err(e) => {
                ctx.try_log(|logger| {
                    slog::warn!(
                        logger,
                        "unable to retrieve_full_block_breakdown: {}",
                        e.to_string()
                    )
                });
                return Json(json!({
                    "status": 500,
                    "result": "unable to retrieve_full_block",
                }));
            }
        };

    let header = block.get_block_header();
    match background_job_tx.lock() {
        Ok(tx) => {
            let _ = tx.send(ObserverCommand::ProcessBitcoinBlock(block));
        }
        Err(e) => {
            ctx.try_log(|logger| {
                slog::warn!(
                    logger,
                    "unable to acquire background_job_tx: {}",
                    e.to_string()
                )
            });
            return Json(json!({
                "status": 500,
                "result": "Unable to acquire lock",
            }));
        }
    };

    let chain_update = match indexer_rw_lock.inner().write() {
        Ok(mut indexer) => indexer.handle_bitcoin_header(header, &ctx),
        Err(e) => {
            ctx.try_log(|logger| {
                slog::warn!(
                    logger,
                    "unable to acquire indexer_rw_lock: {}",
                    e.to_string()
                )
            });
            return Json(json!({
                "status": 500,
                "result": "Unable to acquire lock",
            }));
        }
    };

    match chain_update {
        Ok(Some(chain_event)) => {
            match background_job_tx.lock() {
                Ok(tx) => {
                    let _ = tx.send(ObserverCommand::PropagateBitcoinChainEvent(chain_event));
                }
                Err(e) => {
                    ctx.try_log(|logger| {
                        slog::warn!(
                            logger,
                            "unable to acquire background_job_tx: {}",
                            e.to_string()
                        )
                    });
                    return Json(json!({
                        "status": 500,
                        "result": "Unable to acquire lock",
                    }));
                }
            };
        }
        Ok(None) => {
            ctx.try_log(|logger| slog::info!(logger, "unable to infer chain progress"));
        }
        Err(e) => {
            ctx.try_log(|logger| slog::error!(logger, "unable to handle bitcoin block: {}", e))
        }
    }

    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(skip)]
#[post("/new_block", format = "application/json", data = "<marshalled_block>")]
pub fn handle_new_stacks_block(
    indexer_rw_lock: &State<Arc<RwLock<Indexer>>>,
    marshalled_block: Json<JsonValue>,
    background_job_tx: &State<Arc<Mutex<Sender<ObserverCommand>>>>,
    ctx: &State<Context>,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /new_block"));
    // Standardize the structure of the block, and identify the
    // kind of update that this new block would imply, taking
    // into account the last 7 blocks.
    // TODO(lgalabru): use _pox_info
    let (_pox_info, chain_event) = match indexer_rw_lock.inner().write() {
        Ok(mut indexer) => {
            let pox_info = indexer.get_pox_info();
            let chain_event =
                indexer.handle_stacks_marshalled_block(marshalled_block.into_inner(), &ctx);
            (pox_info, chain_event)
        }
        Err(e) => {
            ctx.try_log(|logger| {
                slog::warn!(
                    logger,
                    "unable to acquire indexer_rw_lock: {}",
                    e.to_string()
                )
            });
            return Json(json!({
                "status": 500,
                "result": "Unable to acquire lock",
            }));
        }
    };

    match chain_event {
        Ok(Some(chain_event)) => {
            let background_job_tx = background_job_tx.inner();
            match background_job_tx.lock() {
                Ok(tx) => {
                    let _ = tx.send(ObserverCommand::PropagateStacksChainEvent(chain_event));
                }
                Err(e) => {
                    ctx.try_log(|logger| {
                        slog::warn!(
                            logger,
                            "unable to acquire background_job_tx: {}",
                            e.to_string()
                        )
                    });
                    return Json(json!({
                        "status": 500,
                        "result": "Unable to acquire lock",
                    }));
                }
            };
        }
        Ok(None) => {
            ctx.try_log(|logger| slog::info!(logger, "unable to infer chain progress"));
        }
        Err(e) => ctx.try_log(|logger| slog::error!(logger, "{}", e)),
    }

    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(skip)]
#[post(
    "/new_microblocks",
    format = "application/json",
    data = "<marshalled_microblock>"
)]
pub fn handle_new_microblocks(
    indexer_rw_lock: &State<Arc<RwLock<Indexer>>>,
    marshalled_microblock: Json<JsonValue>,
    background_job_tx: &State<Arc<Mutex<Sender<ObserverCommand>>>>,
    ctx: &State<Context>,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /new_microblocks"));
    // Standardize the structure of the microblock, and identify the
    // kind of update that this new microblock would imply
    let chain_event = match indexer_rw_lock.inner().write() {
        Ok(mut indexer) => {
            let chain_event = indexer.handle_stacks_marshalled_microblock_trail(
                marshalled_microblock.into_inner(),
                &ctx,
            );
            chain_event
        }
        Err(e) => {
            ctx.try_log(|logger| {
                slog::warn!(
                    logger,
                    "unable to acquire background_job_tx: {}",
                    e.to_string()
                )
            });
            return Json(json!({
                "status": 500,
                "result": "Unable to acquire lock",
            }));
        }
    };

    match chain_event {
        Ok(Some(chain_event)) => {
            let background_job_tx = background_job_tx.inner();
            match background_job_tx.lock() {
                Ok(tx) => {
                    let _ = tx.send(ObserverCommand::PropagateStacksChainEvent(chain_event));
                }
                Err(e) => {
                    ctx.try_log(|logger| {
                        slog::warn!(
                            logger,
                            "unable to acquire background_job_tx: {}",
                            e.to_string()
                        )
                    });
                    return Json(json!({
                        "status": 500,
                        "result": "Unable to acquire lock",
                    }));
                }
            };
        }
        Ok(None) => {
            ctx.try_log(|logger| slog::info!(logger, "unable to infer chain progress"));
        }
        Err(e) => {
            ctx.try_log(|logger| slog::error!(logger, "unable to handle stacks microblock: {}", e));
        }
    }

    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(skip)]
#[post("/new_mempool_tx", format = "application/json", data = "<raw_txs>")]
pub fn handle_new_mempool_tx(
    raw_txs: Json<Vec<String>>,
    background_job_tx: &State<Arc<Mutex<Sender<ObserverCommand>>>>,
    ctx: &State<Context>,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /new_mempool_tx"));
    let transactions = raw_txs
        .iter()
        .map(|tx_data| {
            let (tx_description, ..) = indexer::stacks::get_tx_description(&tx_data, &vec![])
                .expect("unable to parse transaction");
            MempoolAdmissionData {
                tx_data: tx_data.clone(),
                tx_description,
            }
        })
        .collect::<Vec<_>>();

    let background_job_tx = background_job_tx.inner();
    match background_job_tx.lock() {
        Ok(tx) => {
            let _ = tx.send(ObserverCommand::PropagateStacksMempoolEvent(
                StacksChainMempoolEvent::TransactionsAdmitted(transactions),
            ));
        }
        _ => {}
    };

    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(skip)]
#[post("/drop_mempool_tx", format = "application/json")]
pub fn handle_drop_mempool_tx(ctx: &State<Context>) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /drop_mempool_tx"));
    // TODO(lgalabru): use propagate mempool events
    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(skip)]
#[post("/attachments/new", format = "application/json")]
pub fn handle_new_attachement(ctx: &State<Context>) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /attachments/new"));
    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(skip)]
#[post("/mined_block", format = "application/json", data = "<payload>")]
pub fn handle_mined_block(payload: Json<JsonValue>, ctx: &State<Context>) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /mined_block {:?}", payload));
    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(skip)]
#[post("/mined_microblock", format = "application/json", data = "<payload>")]
pub fn handle_mined_microblock(payload: Json<JsonValue>, ctx: &State<Context>) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /mined_microblock {:?}", payload));
    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(skip)]
#[post("/wallet", format = "application/json", data = "<bitcoin_rpc_call>")]
pub async fn handle_bitcoin_wallet_rpc_call(
    bitcoin_config: &State<BitcoinConfig>,
    bitcoin_rpc_call: Json<BitcoinRPCRequest>,
    ctx: &State<Context>,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /wallet"));

    use base64::encode;
    use reqwest::Client;

    let bitcoin_rpc_call = bitcoin_rpc_call.into_inner().clone();

    let body = rocket::serde::json::serde_json::to_vec(&bitcoin_rpc_call).unwrap_or(vec![]);

    let token = encode(format!(
        "{}:{}",
        bitcoin_config.username, bitcoin_config.password
    ));

    let url = format!("{}", bitcoin_config.rpc_url);
    let client = Client::new();
    let builder = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Basic {}", token))
        .timeout(std::time::Duration::from_secs(5));

    match builder.body(body).send().await {
        Ok(res) => Json(res.json().await.unwrap()),
        Err(_) => Json(json!({
            "status": 500
        })),
    }
}

#[openapi(skip)]
#[post("/", format = "application/json", data = "<bitcoin_rpc_call>")]
pub async fn handle_bitcoin_rpc_call(
    bitcoin_config: &State<BitcoinConfig>,
    bitcoin_rpc_call: Json<BitcoinRPCRequest>,
    background_job_tx: &State<Arc<Mutex<Sender<ObserverCommand>>>>,
    ctx: &State<Context>,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /"));

    use base64::encode;
    use reqwest::Client;

    let bitcoin_rpc_call = bitcoin_rpc_call.into_inner().clone();
    let method = bitcoin_rpc_call.method.clone();

    let body = rocket::serde::json::serde_json::to_vec(&bitcoin_rpc_call).unwrap_or(vec![]);

    let token = encode(format!(
        "{}:{}",
        bitcoin_config.username, bitcoin_config.password
    ));

    ctx.try_log(|logger| {
        slog::debug!(
            logger,
            "Forwarding {} request to {}",
            method,
            bitcoin_config.rpc_url
        )
    });

    let url = if method == "listunspent" {
        format!("{}/wallet/", bitcoin_config.rpc_url)
    } else {
        bitcoin_config.rpc_url.to_string()
    };

    let client = Client::new();
    let builder = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Basic {}", token))
        .timeout(std::time::Duration::from_secs(5));

    if method == "sendrawtransaction" {
        let background_job_tx = background_job_tx.inner();
        match background_job_tx.lock() {
            Ok(tx) => {
                let _ = tx.send(ObserverCommand::NotifyBitcoinTransactionProxied);
            }
            _ => {}
        };
    }

    match builder.body(body).send().await {
        Ok(res) => {
            let payload = res.json().await.unwrap();
            ctx.try_log(|logger| slog::debug!(logger, "Responding with response {:?}", payload));
            Json(payload)
        }
        Err(_) => Json(json!({
            "status": 500
        })),
    }
}

#[openapi(tag = "Chainhooks")]
#[get("/v1/chainhooks", format = "application/json")]
pub fn handle_get_hooks(
    chainhook_store: &State<Arc<RwLock<ChainhookStore>>>,
    ctx: &State<Context>,
    api_key: ApiKey,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "GET /v1/chainhooks"));
    if let Ok(chainhook_store_reader) = chainhook_store.inner().read() {
        match chainhook_store_reader.entries.get(&api_key) {
            None => {
                ctx.try_log(|logger| {
                    slog::info!(
                        logger,
                        "No chainhook registered for api key {:?}",
                        api_key.0
                    )
                });
                Json(json!({
                    "status": 404,
                }))
            }
            Some(hooks) => {
                let mut predicates = vec![];
                let mut stacks_predicates = hooks
                    .get_serialized_stacks_predicates()
                    .iter()
                    .map(|(uuid, network, predicate)| {
                        json!({
                            "chain": "stacks",
                            "uuid": uuid,
                            "network": network,
                            "predicate": predicate,
                        })
                    })
                    .collect::<Vec<_>>();
                predicates.append(&mut stacks_predicates);
                let mut bitcoin_predicates = hooks
                    .get_serialized_bitcoin_predicates()
                    .iter()
                    .map(|(uuid, network, predicate)| {
                        json!({
                            "chain": "bitcoin",
                            "uuid": uuid,
                            "network": network,
                            "predicate": predicate,
                        })
                    })
                    .collect::<Vec<_>>();
                predicates.append(&mut bitcoin_predicates);

                Json(json!({
                    "status": 200,
                    "result": predicates
                }))
            }
        }
    } else {
        Json(json!({
            "status": 500,
            "message": "too many requests",
        }))
    }
}

#[openapi(tag = "Chainhooks")]
#[post("/v1/chainhooks", format = "application/json", data = "<hook>")]
pub fn handle_create_hook(
    hook: Json<ChainhookFullSpecification>,
    background_job_tx: &State<Arc<Mutex<Sender<ObserverCommand>>>>,
    ctx: &State<Context>,
    api_key: ApiKey,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "POST /v1/chainhooks"));
    let hook = hook.into_inner();
    if let Err(e) = hook.validate() {
        return Json(json!({
            "status": 422,
            "error": e,
        }));
    }

    let background_job_tx = background_job_tx.inner();
    match background_job_tx.lock() {
        Ok(tx) => {
            let _ = tx.send(ObserverCommand::RegisterHook(hook, api_key));
        }
        _ => {}
    };

    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(tag = "Chainhooks")]
#[delete("/v1/chainhooks/stacks/<hook_uuid>", format = "application/json")]
pub fn handle_delete_stacks_hook(
    hook_uuid: String,
    background_job_tx: &State<Arc<Mutex<Sender<ObserverCommand>>>>,
    ctx: &State<Context>,
    api_key: ApiKey,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "DELETE /v1/chainhooks/stacks/<hook_uuid>"));

    let background_job_tx = background_job_tx.inner();
    match background_job_tx.lock() {
        Ok(tx) => {
            let _ = tx.send(ObserverCommand::DeregisterStacksHook(hook_uuid, api_key));
        }
        _ => {}
    };

    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[openapi(tag = "Chainhooks")]
#[delete("/v1/chainhooks/bitcoin/<hook_uuid>", format = "application/json")]
pub fn handle_delete_bitcoin_hook(
    hook_uuid: String,
    background_job_tx: &State<Arc<Mutex<Sender<ObserverCommand>>>>,
    ctx: &State<Context>,
    api_key: ApiKey,
) -> Json<JsonValue> {
    ctx.try_log(|logger| slog::info!(logger, "DELETE /v1/chainhooks/stacks/<hook_uuid>"));

    let background_job_tx = background_job_tx.inner();
    match background_job_tx.lock() {
        Ok(tx) => {
            let _ = tx.send(ObserverCommand::DeregisterBitcoinHook(hook_uuid, api_key));
        }
        _ => {}
    };

    Json(json!({
        "status": 200,
        "result": "Ok",
    }))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, OpenApiFromRequest)]
pub struct ApiKey(pub Option<String>);

#[derive(Debug)]
pub enum ApiKeyError {
    Missing,
    Invalid,
    InternalError,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ApiKey {
    type Error = ApiKeyError;

    async fn from_request(req: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        let state = req.rocket().state::<Arc<RwLock<ChainhookStore>>>();
        if let Some(chainhook_store_handle) = state {
            if let Ok(chainhook_store_reader) = chainhook_store_handle.read() {
                let key = req.headers().get_one("x-api-key");
                match key {
                    Some(key) => {
                        match chainhook_store_reader.is_authorized(Some(key.to_string())) {
                            true => Outcome::Success(ApiKey(Some(key.to_string()))),
                            false => Outcome::Failure((Status::BadRequest, ApiKeyError::Invalid)),
                        }
                    }
                    None => match chainhook_store_reader.is_authorized(None) {
                        true => Outcome::Success(ApiKey(None)),
                        false => Outcome::Failure((Status::BadRequest, ApiKeyError::Invalid)),
                    },
                }
            } else {
                Outcome::Failure((Status::InternalServerError, ApiKeyError::InternalError))
            }
        } else {
            Outcome::Failure((Status::InternalServerError, ApiKeyError::InternalError))
        }
    }
}

#[cfg(test)]
mod tests;
