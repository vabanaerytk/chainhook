pub fn generate_config() -> String {
    let conf = format!(
        r#"
[storage]
driver = "redis"
redis_uri = "redis://localhost:6379/"

[chainhooks]
max_stacks_registrations = 500
max_bitcoin_registrations = 500

[network]
mode = "devnet"
bitcoin_node_rpc_url = "http://0.0.0.0:18443"
bitcoin_node_rpc_username = "devnet"
bitcoin_node_rpc_password = "devnet"
stacks_node_rpc_url = "http://0.0.0.0:20443"
"#
    );
    return conf;
}
