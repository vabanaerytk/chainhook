use chainhook_sdk::types::BitcoinNetwork;

pub fn generate_config(network: &BitcoinNetwork) -> String {
    let network = format!("{:?}", network);
    let conf = format!(
        r#"[storage]
working_dir = "cache"

# The Http Api allows you to register / deregister
# dynamically predicates.
# Disable by default.
#
# [http_api]
# http_port = 20456
# database_uri = "redis://localhost:6379/"

[network]
mode = "{network}"
bitcoind_rpc_url = "http://localhost:8332"
bitcoind_rpc_username = "devnet"
bitcoind_rpc_password = "devnet"

# Chainhook must be able to receive Bitcoin block events.
# These events can originate from either a Stacks node or a Bitcoin node's ZeroMQ interface.

# By default, the service is set to receive Bitcoin block events from the Stacks node (via burnchain):
stacks_node_rpc_url = "http://localhost:20443"

# However, events can also be received directly from a Bitcoin node.
# To achieve this, comment out the `stacks_node_rpc_url` line and uncomment the following line:
# bitcoind_zmq_url = "tcp://0.0.0.0:18543"

[limits]
max_number_of_bitcoin_predicates = 100
max_number_of_concurrent_bitcoin_scans = 100
max_number_of_stacks_predicates = 10
max_number_of_concurrent_stacks_scans = 10
max_number_of_processing_threads = 16
max_number_of_networking_threads = 16
max_caching_memory_size_mb = 32000

# If you don't require historical data for your predicates or wish to avoid downloading 
# the archive data every time, you may comment out the `tsv_file_url` line.
#
[[event_source]]
tsv_file_url = "https://archive.hiro.so/{network}/stacks-blockchain-api/{network}-stacks-blockchain-api-latest"
"#,
        network = network.to_lowercase(),
    );
    return conf;
}
