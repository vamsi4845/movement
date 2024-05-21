pub mod v1;

pub use aptos_types::{
    transaction::signature_verified_transaction::SignatureVerifiedTransaction,
    block_executor::partitioner::ExecutableBlock,
    block_executor::partitioner::ExecutableTransactions,
    transaction::{SignedTransaction, Transaction}
};
pub use aptos_crypto::hash::HashValue;
use aptos_api::runtime::Apis;

pub use maptos_execution_util::FinalityMode;
use movement_types::BlockCommitment;

use async_channel::Sender;

#[tonic::async_trait]
pub trait MonzaExecutor {

    /// Runs the service
    async fn run_service(&self) -> Result<(), anyhow::Error>;

    /// Runs the necessary background tasks.
    async fn run_background_tasks(&self) -> Result<(), anyhow::Error>;

    /// Executes a block dynamically
    async fn execute_block(
        &self,
        mode: FinalityMode, 
        block: ExecutableBlock,
    ) -> Result<BlockCommitment, anyhow::Error>;

	/// Sets the transaction channel.
	fn set_tx_channel(
		&mut self,
		tx_channel: Sender<SignedTransaction>,
	);

	/// Gets the dyn API.
	fn get_api(&self, mode: FinalityMode) -> Apis;

    /// Get block head height.
    async fn get_block_head_height(&self) -> Result<u64, anyhow::Error>;
    
}
