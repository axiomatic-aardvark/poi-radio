use colored::*;
use ethers::signers::LocalWallet;
use ethers::types::Block;
use ethers::{
    providers::{Http, Middleware, Provider},
    types::U64,
};
use graphcast_sdk::gossip_agent::GossipAgent;
use graphcast_sdk::graphql::client_network::query_network_subgraph;
use graphcast_sdk::graphql::client_registry::query_registry_indexer;
use graphcast_sdk::{init_tracing, operator_address, read_boot_node_addresses};
use num_bigint::BigUint;
use num_traits::Zero;
use poi_radio::{
    active_allocation_hashes, attestation_handler, compare_attestations, process_messages,
    save_local_attestation, Attestation, LocalAttestationsMap, RadioPayloadMessage, GOSSIP_AGENT,
    MESSAGES,
};
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use std::{thread::sleep, time::Duration};
use tracing::{debug, error, info};

use dotenv::dotenv;
/// Radio specific query function to fetch Proof of Indexing for each allocated subgraph
use graphql::query_graph_node_poi;

mod graphql;

#[macro_use]
extern crate partial_application;

#[tokio::main]
async fn main() {
    dotenv().ok();
    init_tracing();

    let graph_node_endpoint =
        env::var("GRAPH_NODE_STATUS_ENDPOINT").expect("No Graph node status endpoint provided.");
    let private_key = env::var("PRIVATE_KEY").expect("No private key provided.");
    let eth_node = env::var("ETH_NODE").expect("No ETH URL provided.");

    // Option for where to host the waku node instance
    let waku_host = env::var("WAKU_HOST").ok();
    let waku_port = env::var("WAKU_PORT").ok();

    // Subgraph endpoints
    let registry_subgraph =
        env::var("REGISTRY_SUBGRAPH").expect("No registry subgraph endpoint provided.");
    let network_subgraph =
        env::var("NETWORK_SUBGRAPH").expect("No network subgraph endpoint provided.");

    // Send message every x blocks for which wait y blocks before attestations
    let examination_frequency = 3;
    let wait_block_duration = 2;

    let provider: Provider<Http> = Provider::<Http>::try_from(eth_node.clone()).unwrap();
    let wallet = private_key.parse::<LocalWallet>().unwrap();
    let radio_name: &str = "poi-radio";
    let topics = active_allocation_hashes(operator_address(&wallet))
        .await
        .ok();

    let gossip_agent = GossipAgent::new(
        private_key,
        eth_node,
        radio_name,
        &registry_subgraph,
        &network_subgraph,
        read_boot_node_addresses(),
        topics,
        waku_host,
        waku_port,
        None,
    )
    .await
    .unwrap();

    _ = GOSSIP_AGENT.set(gossip_agent);
    _ = MESSAGES.set(Arc::new(Mutex::new(vec![])));

    let radio_handler = Arc::new(Mutex::new(attestation_handler()));
    GOSSIP_AGENT.get().unwrap().register_handler(radio_handler);

    let mut curr_block = 0;
    let mut compare_block: u64 = 0;

    let local_attestations: Arc<Mutex<LocalAttestationsMap>> = Arc::new(Mutex::new(HashMap::new()));

    // Main loop for sending messages, can factor out
    // and take radio specific query and parsing for radioPayload
    loop {
        let block_number = U64::as_u64(&provider.get_block_number().await.unwrap()) - 5;

        if curr_block == block_number {
            sleep(Duration::from_secs(5));
            continue;
        }

        debug!("{} {}", "🔗 Block number:".cyan(), block_number);
        curr_block = block_number;

        if block_number == compare_block {
            debug!("{}", "Comparing attestations".magenta());

            let remote_attestations = process_messages(Arc::clone(MESSAGES.get().unwrap())).await;
            match remote_attestations {
                Ok(remote_attestations) => {
                    let mut messages = MESSAGES.get().unwrap().lock().unwrap();
                    match compare_attestations(
                        compare_block - wait_block_duration,
                        remote_attestations,
                        Arc::clone(&local_attestations),
                    ) {
                        Ok(msg) => {
                            debug!("{}", msg.green().bold());
                            messages.clear();
                        }
                        Err(err) => {
                            error!("{}", err);
                            messages.clear();
                        }
                    }
                }
                Err(err) => {
                    error!(
                        "{}{}",
                        "An error occured while parsing messages: {}".red().bold(),
                        err
                    );
                }
            }
        }

        let my_address =
            query_registry_indexer(registry_subgraph.to_string(), operator_address(&wallet))
                .await
                .ok();
        let my_stake = if let Some(addr) = my_address.clone() {
            query_network_subgraph(network_subgraph.to_string(), addr)
                .await
                .unwrap()
                .indexer_stake()
        } else {
            BigUint::zero()
        };
        info!(
            "Acting on behave of indexer {:#?} with stake {}",
            my_address, my_stake
        );

        // Send POI message at a fixed frequency
        if block_number % examination_frequency == 0 {
            compare_block = block_number + wait_block_duration;

            let block: Block<_> = provider.get_block(block_number).await.unwrap().unwrap();
            let block_hash = format!("{:#x}", block.hash.unwrap());

            // Radio specific message content query function
            // Function takes in an identifier string and make specific queries regarding the identifier
            // The example here combines a single function provided query endpoint, current block info
            // Then the function gets sent to agent for making identifier independent queries
            let poi_query = partial!( query_graph_node_poi => graph_node_endpoint.clone(), _, block_hash.to_string(),block_number.try_into().unwrap());
            let identifiers = GOSSIP_AGENT.get().unwrap().content_identifiers();

            for id in identifiers {
                match poi_query(id.clone()).await {
                    Ok(content) => {
                        let attestation = Attestation {
                            npoi: content.clone(),
                            stake_weight: my_stake.clone(),
                            senders: Vec::new(),
                        };

                        save_local_attestation(
                            &mut local_attestations.lock().unwrap(),
                            attestation,
                            id.clone(),
                            block_number,
                        );

                        let radio_message = RadioPayloadMessage::new(id.clone(), content.clone());
                        match GOSSIP_AGENT
                            .get()
                            .unwrap()
                            .send_message(id.clone(), block_number, Some(radio_message))
                            .await
                        {
                            Ok(sent) => info!("{}: {}", "Sent message id:".green(), sent),
                            Err(e) => error!("{}: {}", "Failed to send message".red(), e),
                        };
                    }
                    Err(e) => error!("{}: {}", "Failed to query message".red(), e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex::encode;
    use rand::{thread_rng, Rng};
    use secp256k1::SecretKey;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    #[ignore]
    async fn regression_test() {
        dotenv().ok();

        let is_display = matches!(std::env::args().nth(1), Some(x) if x == *"display");
        let mut rng = thread_rng();
        let mut private_key = [0u8; 32];
        rng.fill(&mut private_key[..]);

        let private_key = SecretKey::from_slice(&private_key).expect("Error parsing secret key");
        let private_key_hex = encode(private_key.secret_bytes());
        env::set_var("PRIVATE_KEY", &private_key_hex);

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/gossip-registry-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{
                    "data": {
                        "graphAccount": {
                            "gossipOperatorOf": {
                                "id": "0x54f4cdc1ac7cd3377f43834fbde09a7ffe6fe337"
                            }
                        }
                    },
                    "errors": null
                }"#,
            ))
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/network-subgraph"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{
                    "data": {
                        "indexer" : {
                            "stakedTokens": "100000000000000000000000",
                            "allocations": [{
                                "subgraphDeployment": {
                                    "ipfsHash": "QmbaLc7fEfLGUioKWehRhq838rRzeR8cBoapNJWNSAZE8u"
                                }
                            }]
                        },
                        "graphNetwork": {
                            "minimumIndexerStake": "100000000000000000000000"
                        }
                    },
                    "errors": null
                }"#,
            ))
            .mount(&mock_server)
            .await;

        let private_key = env::var("PRIVATE_KEY").expect("No private key provided.");
        let eth_node = env::var("ETH_NODE").expect("No ETH URL provided.");

        // TODO: Add something random and unique here to avoid noise form other operators
        let radio_name: &str = "test-poi-crosschecker-radio";

        let gossip_agent = GossipAgent::new(
            private_key,
            eth_node,
            radio_name,
            &(mock_server.uri() + "/gossip-registry-test"),
            &(mock_server.uri() + "/network-subgraph"),
            [].to_vec(),
            Some(vec!["some-hash".to_string()]),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        _ = GOSSIP_AGENT.set(gossip_agent);
        _ = MESSAGES.set(Arc::new(Mutex::new(vec![])));

        let radio_handler = Arc::new(Mutex::new(attestation_handler()));
        GOSSIP_AGENT.get().unwrap().register_handler(radio_handler);
        let hash = "some-hash".to_string();
        let content = "poi".to_string();

        let radio_msg = RadioPayloadMessage::new(hash.clone(), content.clone());
        // Just to introduce sender and skip first time check
        GOSSIP_AGENT
            .get()
            .unwrap()
            .send_message("some-hash".to_string(), 0, Some(radio_msg.clone()))
            .await
            .unwrap();

        sleep(Duration::from_secs(1));

        let mut block = 1;

        loop {
            GOSSIP_AGENT
                .get()
                .unwrap()
                .send_message("some-hash".to_string(), block, Some(radio_msg.clone()))
                .await
                .unwrap();

            if is_display && MESSAGES.get().unwrap().lock().unwrap().len() > 4 {
                break;
            }

            block += 1;
            sleep(Duration::from_secs(1));
        }
    }
}
