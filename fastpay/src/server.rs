// Copyright (c) Facebook, Inc. and its affiliates.
// SPDX-License-Identifier: Apache-2.0

#![deny(warnings)]

use fastpay::{config::*, network, transport};
use fastpay_core::{account::AccountState, authority::*, base_types::*, committee::CoconutSetup};

use futures::future::join_all;
use log::*;
use std::{
    path::{Path, PathBuf},
    str::FromStr,
};
use structopt::StructOpt;
use tokio::runtime::Runtime;

#[allow(clippy::too_many_arguments)]
fn make_shard_server(
    local_ip_addr: &str,
    server_config_path: &Path,
    committee_config_path: &Path,
    initial_accounts_config_path: &Path,
    buffer_size: usize,
    cross_shard_config: network::CrossShardConfig,
    shard: u32,
) -> network::Server {
    let server_config =
        AuthorityServerConfig::read(server_config_path).expect("Fail to read server config");
    let committee_config =
        CommitteeConfig::read(committee_config_path).expect("Fail to read committee config");
    let initial_accounts_config = InitialStateConfig::read(initial_accounts_config_path)
        .expect("Fail to read initial account config");

    let committee = committee_config.into_committee();
    let num_shards = server_config.authority.num_shards;

    let mut state = AuthorityState::new_shard(
        committee,
        server_config.key,
        server_config.coconut_key,
        shard,
        num_shards,
    );

    // Load initial states
    for (id, owner, balance) in &initial_accounts_config.accounts {
        if AuthorityState::get_shard(num_shards, id) != shard {
            continue;
        }
        let client = AccountState::new(*owner, *balance);
        state.accounts.insert(id.clone(), client);
    }

    network::Server::new(
        server_config.authority.network_protocol,
        local_ip_addr.to_string(),
        server_config.authority.base_port,
        state,
        buffer_size,
        cross_shard_config,
    )
}

fn make_servers(
    local_ip_addr: &str,
    server_config_path: &Path,
    committee_config_path: &Path,
    initial_accounts_config_path: &Path,
    buffer_size: usize,
    cross_shard_config: network::CrossShardConfig,
) -> Vec<network::Server> {
    let server_config =
        AuthorityServerConfig::read(server_config_path).expect("Fail to read server config");
    let num_shards = server_config.authority.num_shards;

    let mut servers = Vec::new();
    for shard in 0..num_shards {
        servers.push(make_shard_server(
            local_ip_addr,
            server_config_path,
            committee_config_path,
            initial_accounts_config_path,
            buffer_size,
            cross_shard_config.clone(),
            shard,
        ))
    }
    servers
}

#[derive(StructOpt)]
#[structopt(
    name = "FastPay Server",
    about = "A byzantine fault tolerant payments sidechain with low-latency finality and high throughput"
)]
struct ServerOptions {
    /// Subcommands. Acceptable values are run and generate.
    #[structopt(subcommand)]
    cmd: ServerCommands,
}

#[derive(StructOpt, Debug, PartialEq, Eq)]
struct AuthorityOptions {
    /// Path to the file containing the server configuration of this FastPay authority (including its secret key)
    #[structopt(long = "server")]
    server_config_path: PathBuf,

    /// Chooses a network protocol between Udp and Tcp
    #[structopt(long, default_value = "Udp")]
    protocol: transport::NetworkProtocol,

    /// Sets the public name of the host
    #[structopt(long)]
    host: String,

    /// Sets the base port, i.e. the port on which the server listens for the first shard
    #[structopt(long)]
    port: u32,

    /// Number of shards for this authority
    #[structopt(long)]
    shards: u32,
}

impl FromStr for AuthorityOptions {
    type Err = failure::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(':').collect();
        failure::ensure!(
            parts.len() == 5,
            "Expecting format `file.json:(udp|tcp):host:port:num-shards`"
        );

        let server_config_path = Path::new(parts[0]).to_path_buf();
        let protocol = parts[1]
            .parse()
            .map_err(|s| failure::format_err!("{}", s))?;
        let host = parts[2].to_string();
        let port = parts[3].parse()?;
        let shards = parts[4].parse()?;

        Ok(Self {
            server_config_path,
            protocol,
            host,
            port,
            shards,
        })
    }
}

fn make_server_config(options: AuthorityOptions) -> AuthorityServerConfig {
    let key = KeyPair::generate();
    let name = key.public();
    let authority = AuthorityConfig {
        network_protocol: options.protocol,
        name,
        host: options.host,
        base_port: options.port,
        num_shards: options.shards,
    };
    AuthorityServerConfig {
        authority,
        key,
        coconut_key: None,
    }
}

#[derive(StructOpt)]
enum ServerCommands {
    /// Runs a service for each shard of the FastPay authority")
    #[structopt(name = "run")]
    Run {
        /// Path to the file containing the server configuration of this FastPay authority (including its secret key)
        #[structopt(long = "server")]
        server_config_path: PathBuf,

        /// Maximum size of datagrams received and sent (bytes)
        #[structopt(long, default_value = transport::DEFAULT_MAX_DATAGRAM_SIZE)]
        buffer_size: usize,

        /// Configuration for cross shard requests
        #[structopt(flatten)]
        cross_shard_config: network::CrossShardConfig,

        /// Path to the file containing the public description of all authorities in this FastPay committee
        #[structopt(long)]
        committee: PathBuf,

        /// Path to the file describing the initial user accounts
        #[structopt(long)]
        initial_accounts: PathBuf,

        /// Runs a specific shard (from 0 to shards-1)
        #[structopt(long)]
        shard: Option<u32>,
    },

    /// Generate a new server configuration and output its public description
    #[structopt(name = "generate")]
    Generate {
        #[structopt(flatten)]
        options: AuthorityOptions,
    },

    /// Act as a trusted third-party and generate all server configurations
    #[structopt(name = "generate-all")]
    GenerateAll {
        /// Configuration of each authority in the committee encoded as `(Udp|Tcp):host:port:num-shards`
        #[structopt(long)]
        authorities: Vec<AuthorityOptions>,

        /// Path where to write the description of the FastPay committee
        #[structopt(long)]
        committee: PathBuf,
    },
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = ServerOptions::from_args();

    match options.cmd {
        ServerCommands::Run {
            server_config_path,
            buffer_size,
            cross_shard_config,
            committee,
            initial_accounts,
            shard,
        } => {
            // Run the server
            let servers = match shard {
                Some(shard) => {
                    info!("Running shard number {}", shard);
                    let server = make_shard_server(
                        "0.0.0.0", // Allow local IP address to be different from the public one.
                        &server_config_path,
                        &committee,
                        &initial_accounts,
                        buffer_size,
                        cross_shard_config,
                        shard,
                    );
                    vec![server]
                }
                None => {
                    info!("Running all shards");
                    make_servers(
                        "0.0.0.0", // Allow local IP address to be different from the public one.
                        &server_config_path,
                        &committee,
                        &initial_accounts,
                        buffer_size,
                        cross_shard_config,
                    )
                }
            };

            let rt = Runtime::new().unwrap();
            let mut handles = Vec::new();
            for server in servers {
                handles.push(async move {
                    let spawned_server = match server.spawn().await {
                        Ok(server) => server,
                        Err(err) => {
                            error!("Failed to start server: {}", err);
                            return;
                        }
                    };
                    if let Err(err) = spawned_server.join().await {
                        error!("Server ended with an error: {}", err);
                    }
                });
            }
            rt.block_on(join_all(handles));
        }

        ServerCommands::Generate { options } => {
            let path = options.server_config_path.clone();
            let server = make_server_config(options);
            server
                .write(&path)
                .expect("Unable to write server config file");
            info!("Wrote server config file");
            server.authority.print();
        }

        ServerCommands::GenerateAll {
            authorities,
            committee,
        } => {
            let mut rng = coconut::rand::thread_rng();
            let parameters = coconut::Parameters::new(
                3,
                /* TODO: check party capacity for bulletproofs */ authorities.len(),
            );
            let threshold = (2 * authorities.len() + 1) / 3;
            let (verification_key, key_pairs) =
                coconut::KeyPair::ttp(&mut rng, &parameters, threshold, authorities.len());
            let coconut_setup = CoconutSetup {
                parameters,
                verification_key,
            };
            let authorities = authorities
                .into_iter()
                .zip(key_pairs.into_iter())
                .map(|(options, coconut_key_pair)| {
                    let path = options.server_config_path.clone();
                    let mut server = make_server_config(options);
                    server.coconut_key = Some(coconut_key_pair);
                    server
                        .write(&path)
                        .expect("Unable to write server config file");
                    info!("Wrote server config {}", path.to_str().unwrap());
                    server.authority
                })
                .collect();

            let config = CommitteeConfig {
                authorities,
                coconut_setup: Some(coconut_setup),
            };
            config
                .write(&committee)
                .expect("Unable to write committee description");
            info!("Wrote committee config {}", committee.to_str().unwrap());
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_authority_options() {
        let options = AuthorityOptions::from_str("server.json:udp:localhost:9001:2").unwrap();
        assert_eq!(
            options,
            AuthorityOptions {
                server_config_path: "server.json".into(),
                protocol: transport::NetworkProtocol::Udp,
                host: "localhost".into(),
                port: 9001,
                shards: 2
            }
        );
    }
}
