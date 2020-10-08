//! The consensus component. Provides distributed consensus among the nodes in the network.

mod candidate_block;
mod config;
mod consensus_protocol;
mod era_supervisor;
mod highway_core;
mod protocols;
#[cfg(test)]
mod tests;
mod traits;

use datasize::DataSize;
use std::fmt::{self, Debug, Display, Formatter};

use casper_execution_engine::core::engine_state::era_validators::GetEraValidatorsError;
use casper_types::auction::ValidatorWeights;

use crate::{
    components::{storage::Storage, Component},
    crypto::asymmetric_key::PublicKey,
    effect::{
        announcements::ConsensusAnnouncement,
        requests::{
            self, BlockExecutorRequest, BlockValidationRequest, ContractRuntimeRequest,
            DeployBufferRequest, NetworkRequest, StorageRequest,
        },
        EffectBuilder, Effects,
    },
    protocol::Message,
    types::{BlockHeader, CryptoRngCore, ProtoBlock, Timestamp},
};

pub use config::Config;
pub(crate) use consensus_protocol::{BlockContext, EraEnd};
use derive_more::From;
pub(crate) use era_supervisor::{EraId, EraSupervisor};
use hex_fmt::HexFmt;
use serde::{Deserialize, Serialize};
use tracing::error;
use traits::NodeIdT;

#[derive(Debug, DataSize, Clone, Serialize, Deserialize)]
pub enum ConsensusMessage {
    /// A protocol message, to be handled by the instance in the specified era.
    Protocol { era_id: EraId, payload: Vec<u8> },
    /// A request for evidence against the specified validator, from any era that is still bonded
    /// in `era_id`.
    EvidenceRequest { era_id: EraId, pub_key: PublicKey },
}

/// Consensus component event.
#[derive(DataSize, Debug, From)]
pub enum Event<I> {
    /// An incoming network message.
    MessageReceived { sender: I, msg: ConsensusMessage },
    /// A scheduled event to be handled by a specified era
    Timer { era_id: EraId, timestamp: Timestamp },
    /// We are receiving the data we require to propose a new block
    NewProtoBlock {
        era_id: EraId,
        proto_block: ProtoBlock,
        block_context: BlockContext,
    },
    #[from]
    ConsensusRequest(requests::ConsensusRequest),
    /// The proto-block has been validated and can now be added to the protocol state
    AcceptProtoBlock {
        era_id: EraId,
        proto_block: ProtoBlock,
    },
    /// The proto-block turned out to be invalid, we might want to accuse/punish/... the sender
    InvalidProtoBlock {
        era_id: EraId,
        sender: I,
        proto_block: ProtoBlock,
    },
    /// Response from the Contract Runtime, containing the validators for the new era
    GetValidatorsResponse {
        /// The header of the switch block
        block_header: Box<BlockHeader>,
        get_validators_result: Result<Option<ValidatorWeights>, GetEraValidatorsError>,
    },
}

impl Display for ConsensusMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ConsensusMessage::Protocol { era_id, payload } => {
                write!(f, "protocol message {:10} in {}", HexFmt(payload), era_id)
            }
            ConsensusMessage::EvidenceRequest { era_id, pub_key } => write!(
                f,
                "request for evidence of fault by {} in {} or earlier",
                pub_key, era_id,
            ),
        }
    }
}

impl<I: Debug> Display for Event<I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Event::MessageReceived { sender, msg } => write!(f, "msg from {:?}: {}", sender, msg),
            Event::Timer { era_id, timestamp } => write!(
                f,
                "timer for era {:?} scheduled for timestamp {}",
                era_id, timestamp
            ),
            Event::NewProtoBlock {
                era_id,
                proto_block,
                block_context,
            } => write!(
                f,
                "New proto-block for era {:?}: {:?}, {:?}",
                era_id, proto_block, block_context
            ),
            Event::ConsensusRequest(request) => write!(
                f,
                "A request for consensus component hash been receieved: {:?}",
                request
            ),
            Event::AcceptProtoBlock {
                era_id,
                proto_block,
            } => write!(
                f,
                "A proto-block has been validated for era {:?}: {:?}",
                era_id, proto_block
            ),
            Event::InvalidProtoBlock {
                era_id,
                sender,
                proto_block,
            } => write!(
                f,
                "A proto-block received from {:?} turned out to be invalid for era {:?}: {:?}",
                sender, era_id, proto_block
            ),
            Event::GetValidatorsResponse {
                get_validators_result,
                ..
            } => write!(
                f,
                "We received the response to get_validators from the contract runtime: {:?}",
                get_validators_result
            ),
        }
    }
}

/// A helper trait whose bounds represent the requirements for a reactor event that `EraSupervisor`
/// can work with.
pub trait ReactorEventT<I>:
    From<Event<I>>
    + Send
    + From<NetworkRequest<I, Message>>
    + From<DeployBufferRequest>
    + From<ConsensusAnnouncement>
    + From<BlockExecutorRequest>
    + From<BlockValidationRequest<ProtoBlock, I>>
    + From<StorageRequest<Storage>>
    + From<ContractRuntimeRequest>
{
}

impl<REv, I> ReactorEventT<I> for REv where
    REv: From<Event<I>>
        + Send
        + From<NetworkRequest<I, Message>>
        + From<DeployBufferRequest>
        + From<ConsensusAnnouncement>
        + From<BlockExecutorRequest>
        + From<BlockValidationRequest<ProtoBlock, I>>
        + From<StorageRequest<Storage>>
        + From<ContractRuntimeRequest>
{
}

impl<I, REv> Component<REv> for EraSupervisor<I>
where
    I: NodeIdT,
    REv: ReactorEventT<I>,
{
    type Event = Event<I>;

    fn handle_event(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        mut rng: &mut dyn CryptoRngCore,
        event: Self::Event,
    ) -> Effects<Self::Event> {
        let mut handling_es = self.handling_wrapper(effect_builder, &mut rng);
        match event {
            Event::Timer { era_id, timestamp } => handling_es.handle_timer(era_id, timestamp),
            Event::MessageReceived { sender, msg } => handling_es.handle_message(sender, msg),
            Event::NewProtoBlock {
                era_id,
                proto_block,
                block_context,
            } => handling_es.handle_new_proto_block(era_id, proto_block, block_context),
            Event::ConsensusRequest(requests::ConsensusRequest::HandleLinearBlock(
                block_header,
                responder,
            )) => handling_es.handle_linear_chain_block(*block_header, responder),
            Event::AcceptProtoBlock {
                era_id,
                proto_block,
            } => handling_es.handle_accept_proto_block(era_id, proto_block),
            Event::InvalidProtoBlock {
                era_id,
                sender,
                proto_block,
            } => handling_es.handle_invalid_proto_block(era_id, sender, proto_block),
            Event::GetValidatorsResponse {
                block_header,
                get_validators_result,
            } => match get_validators_result {
                Ok(Some(result)) => {
                    handling_es.handle_get_validators_response(*block_header, result)
                }
                result => {
                    let era_id = block_header.era_id();
                    error!(?result, %era_id, "get_validators returned an error");
                    panic!("couldn't get validators");
                }
            },
        }
    }
}
