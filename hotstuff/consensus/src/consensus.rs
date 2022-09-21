use crate::config::{Committee, Parameters};
use crate::core::Core;
use crate::error::ConsensusError;
use crate::helper::Helper;
use crate::leader::LeaderElector;
use crate::mempool::MempoolDriver;
use crate::messages::{Block, Timeout, Vote, TC};
use crate::proposer::Proposer;
use crate::synchronizer::Synchronizer;
use async_trait::async_trait;
use bytes::Bytes;
use crypto::{Digest, PublicKey, SignatureService};
use futures::SinkExt as _;
use mempool::ConsensusMempoolMessage;
use network::{MessageHandler, Writer};
use serde::{Deserialize, Serialize};
use std::error::Error;
use store::Store;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use tokio::sync::RwLock;
use std::collections::HashMap;
use futures::executor::block_on;
use log::{info};
use utils::monitored_channel::{MonitoredChannel, MonitoredSender};

#[cfg(test)]
#[path = "tests/consensus_tests.rs"]
pub mod consensus_tests;

/// The default channel capacity for each channel of the consensus.
pub const CHANNEL_CAPACITY: usize = 10_000;

/// The consensus round number.
pub type Round = u64;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ConsensusMessage {
    Propose(Block),
    Vote(Vote),
    Timeout(Timeout),
    TC(TC),
    SyncRequest(Digest, PublicKey),
}

pub struct Consensus;

impl Consensus {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        name: PublicKey,
        committee: Committee,
        parameters: Parameters,
        signature_service: SignatureService,
        store: Store,
        rx_mempool: Receiver<Digest>,
        tx_mempool: MonitoredSender<ConsensusMempoolMessage>,
        tx_commit: MonitoredSender<Block>,
        validator_id: u64, 
        consensus_handler_map: Arc<RwLock<HashMap<u64, ConsensusReceiverHandler>>>,
        exit: exit_future::Exit
    ) {
        // NOTE: This log entry is used to compute performance.
        parameters.log();

        // let (tx_consensus, rx_consensus) = channel(CHANNEL_CAPACITY);
        // let (tx_loopback, rx_loopback) = channel(CHANNEL_CAPACITY);
        // let (tx_proposer, rx_proposer) = channel(CHANNEL_CAPACITY);
        // let (tx_helper, rx_helper) = channel(CHANNEL_CAPACITY);

        let (tx_consensus, rx_consensus) = MonitoredChannel::new(CHANNEL_CAPACITY, "consensus-consensus".to_string());
        let (tx_loopback, rx_loopback) = MonitoredChannel::new(CHANNEL_CAPACITY, "consensus-loopback".to_string());
        let (tx_proposer, rx_proposer) = MonitoredChannel::new(CHANNEL_CAPACITY, "consensus-proposer".to_string());
        let (tx_helper, rx_helper) = MonitoredChannel::new(CHANNEL_CAPACITY, "consensus-helper".to_string());


        // Spawn the network receiver.
        // let mut address = committee
        //     .address(&name)
        //     .expect("Our public key is not in the committee");
        // address.set_ip("0.0.0.0".parse().unwrap());
        {
            // Using a thread here avoids blocking the caller due to waiting for the write lock.
            // This might give us a non-fully initialized hotstuff instance because the consensus receiver 
            // handler has not been inserted, but it is worthwhile to save us from the blocking issue.
            tokio::spawn(async move {
                consensus_handler_map
                .write()
                .await
                .insert(validator_id, ConsensusReceiverHandler{tx_consensus, tx_helper});
                info!("Insert consensus handler for validator: {}", validator_id);
            });
        }
        
        // NetworkReceiver::spawn(
        //     address,
        //     /* handler */
        //     ConsensusReceiverHandler {
        //         tx_consensus,
        //         tx_helper,
        //     },
        // );
        // info!(
        //     "Node {} listening to consensus messages on {}",
        //     name, address
        // );

        // Make the leader election module.
        let leader_elector = LeaderElector::new(committee.clone());

        // Make the mempool driver.
        let mempool_driver = MempoolDriver::new(store.clone(), tx_mempool, tx_loopback.clone(), exit.clone());

        // Make the synchronizer.
        let synchronizer = Synchronizer::new(
            name,
            committee.clone(),
            store.clone(),
            tx_loopback.clone(),
            parameters.sync_retry_delay,
            validator_id,
            exit.clone()
        );

        // Spawn the consensus core.
        Core::spawn(
            name,
            committee.clone(),
            signature_service.clone(),
            store.clone(),
            leader_elector,
            mempool_driver,
            synchronizer,
            parameters.timeout_delay,
            /* rx_message */ rx_consensus,
            rx_loopback,
            tx_proposer,
            tx_commit,
            validator_id,
            exit.clone()
        );

        // Spawn the block proposer.
        Proposer::spawn(
            name,
            committee.clone(),
            signature_service,
            rx_mempool,
            /* rx_message */ rx_proposer,
            tx_loopback,
            validator_id,
            exit.clone()
        );

        // Spawn the helper module.
        Helper::spawn(committee, store, /* rx_requests */ rx_helper, validator_id, exit.clone());
    }
}

/// Defines how the network receiver handles incoming primary messages.
#[derive(Clone)]
pub struct ConsensusReceiverHandler {
    tx_consensus: MonitoredSender<ConsensusMessage>,
    tx_helper: MonitoredSender<(Digest, PublicKey)>,
}

#[async_trait]
impl MessageHandler for ConsensusReceiverHandler {
    async fn dispatch(&self, writer: &mut Writer, serialized: Bytes) -> Result<(), Box<dyn Error>> {
        // Deserialize and parse the message.
        match bincode::deserialize(&serialized).map_err(ConsensusError::SerializationError)? {
            ConsensusMessage::SyncRequest(missing, origin) => self
                .tx_helper
                .send((missing, origin))
                .await
                .expect("Failed to send consensus message"),
            message @ ConsensusMessage::Propose(..) => {
                // Reply with an ACK.
                let _ = writer.send(Bytes::from("Ack")).await;

                // Pass the message to the consensus core.
                self.tx_consensus
                    .send(message)
                    .await
                    .expect("Failed to consensus message")
            }
            message => self
                .tx_consensus
                .send(message)
                .await
                .expect("Failed to consensus message"),
        }
        Ok(())
    }
}
