use delegate::delegate;
use ockam_core::async_trait;
use ockam_core::compat::sync::Arc;
use ockam_core::compat::{boxed::Box, vec::Vec};
use ockam_core::errcode::{Kind, Origin};
use ockam_core::{Error, Result};
use ockam_vault::{PublicKey, SecureChannelVault};
use Action::*;
use Event::*;
use Role::*;
use Status::*;

use crate::models::{CredentialAndPurposeKey, Identifier};
use crate::secure_channel::handshake::error::XXError;
use crate::secure_channel::handshake::handshake::Handshake;
use crate::secure_channel::handshake::handshake_state_machine::{
    Action, CommonStateMachine, Event, HandshakeKeys, HandshakeResults, IdentityAndCredentials,
    StateMachine, Status,
};
use crate::{Identities, PurposeKey, Role, TrustContext, TrustPolicy};

/// Implementation of a state machine for the key exchange on the initiator side
#[async_trait]
impl StateMachine for InitiatorStateMachine {
    async fn on_event(&mut self, event: Event) -> Result<Action> {
        let state = self.handshake.state.clone();
        match (state.status, event) {
            // Initialize the handshake and send message 1
            (Initial, Initialize) => {
                self.initialize_handshake().await?;
                let message1 = self.encode_message1(&[]).await?;

                // Send message 1 and wait for message 2
                self.handshake.state.status = WaitingForMessage2;
                Ok(SendMessage(message1))
            }
            // Process message 2 and send message 3
            (WaitingForMessage2, ReceivedMessage(message)) => {
                let message2_payload = self.decode_message2(&message).await?;
                let their_identity_payload: IdentityAndCredentials =
                    minicbor::decode(&message2_payload)?;
                self.verify_identity(their_identity_payload, &self.handshake.state.rs()?.clone())
                    .await?;
                let identity_payload = self
                    .identity_payload
                    .take()
                    .ok_or(XXError::InvalidInternalState)?;
                let message3 = self.encode_message3(&identity_payload).await?;
                self.set_final_state(Initiator).await?;
                Ok(SendMessage(message3))
            }
            // incorrect state / event
            (s, e) => Err(Error::new(
                Origin::Channel,
                Kind::Invalid,
                format!(
                    "Unexpected combination of initiator state and event {:?}/{:?}",
                    s, e
                ),
            )),
        }
    }

    fn get_handshake_results(&self) -> Option<HandshakeResults> {
        self.make_handshake_results(self.get_handshake_keys())
    }
}

/// Implementation of the state machine actions, delegated to the Handshake module
pub(super) struct InitiatorStateMachine {
    pub(super) common: CommonStateMachine,
    pub(super) handshake: Handshake,
    /// this serialized payload contains an identity, its credentials and a signature of its static key
    pub(super) identity_payload: Option<Vec<u8>>,
}

impl InitiatorStateMachine {
    delegate! {
        to self.common {
            async fn verify_identity(&mut self, peer: IdentityAndCredentials, peer_public_key: &PublicKey) -> Result<()>;
            fn make_handshake_results(&self, handshake_keys: Option<HandshakeKeys>) -> Option<HandshakeResults>;
        }
    }
    delegate! {
        to self.handshake {
            #[call(initialize)]
            async fn initialize_handshake(&mut self) -> Result<()>;
            async fn encode_message1(&mut self, payload: &[u8]) -> Result<Vec<u8>>;
            async fn decode_message2(&mut self, message: &[u8]) -> Result<Vec<u8>>;
            async fn encode_message3(&mut self, payload: &[u8]) -> Result<Vec<u8>>;
            async fn set_final_state(&mut self, role: Role) -> Result<()>;
            fn get_handshake_keys(&self) -> Option<HandshakeKeys>;
        }
    }
}

impl InitiatorStateMachine {
    pub async fn new(
        vault: Arc<dyn SecureChannelVault>,
        identities: Arc<Identities>,
        identifier: Identifier,
        purpose_key: PurposeKey,
        credentials: Vec<CredentialAndPurposeKey>,
        trust_policy: Arc<dyn TrustPolicy>,
        trust_context: Option<TrustContext>,
    ) -> Result<InitiatorStateMachine> {
        let common = CommonStateMachine::new(
            identities,
            identifier,
            purpose_key.attestation().clone(),
            credentials,
            trust_policy,
            trust_context,
        );
        let identity_payload = common.make_identity_payload().await?;

        Ok(InitiatorStateMachine {
            common,
            handshake: Handshake::new(vault, purpose_key.key_id().clone()).await?,
            identity_payload: Some(identity_payload),
        })
    }
}
