use crate::{AcceptedBlockCommitment, CommitmentStream, McrSettlementClientOperations};
use alloy_contract::CallBuilder;
use alloy_contract::CallDecoder;
use alloy_network::Ethereum;
use alloy_primitives::Address;
use alloy_provider::fillers::ChainIdFiller;
use alloy_provider::fillers::FillProvider;
use alloy_provider::fillers::GasFiller;
use alloy_provider::fillers::JoinFill;
use alloy_provider::fillers::NonceFiller;
use alloy_provider::fillers::SignerFiller;
use std::array::TryFromSliceError;
//use alloy_provider::fillers::TxFiller;
use alloy_provider::{ProviderBuilder, RootProvider};
use alloy_transport::{Transport, TransportError};
use movement_types::{Commitment, Id};
use std::marker::PhantomData;
use tokio_stream::StreamExt;
//use alloy_network::Network;
use alloy_provider::Provider;
use thiserror::Error;
//use alloy_network::EthereumSigner;
use alloy_primitives::U256;
//use alloy_provider::ProviderBuilder;
use alloy_sol_types::sol;
//use alloy_transport_http::Http;
use alloy::pubsub::PubSubFrontend;
use alloy_network::EthereumSigner;
use alloy_signer_wallet::LocalWallet;
use alloy_transport::BoxTransport;
use alloy_transport_ws::WsConnect;
use movement_types::BlockCommitment;

const MRC_CONTRACT_ADDRESS: &str = "0xBf7c7AE15E23B2E19C7a1e3c36e245A71500e181";
const MAX_TX_SEND_RETRY: usize = 10;
const DEFAULT_TX_GAS_LIMIT: u128 = 10_000_000_000_000_000;

#[derive(Clone, Debug)]
pub struct McrEthSettlementConfig {
	pub mrc_contract_address: String,
	pub gas_limit: u128,
	pub tx_send_nb_retry: usize,
}

impl Default for McrEthSettlementConfig {
	fn default() -> Self {
		McrEthSettlementConfig {
			mrc_contract_address: MRC_CONTRACT_ADDRESS.to_string(),
			gas_limit: DEFAULT_TX_GAS_LIMIT,
			tx_send_nb_retry: MAX_TX_SEND_RETRY,
		}
	}
}

#[derive(Error, Debug)]
pub enum McrEthConnectorError {
	#[error(
		"MCR Settlement Tx fail because gaz estimation is to high. Estimated gaz:{0} gaz limit:{1}"
	)]
	GasLimitExceed(u128, u128),
	#[error("MCR Settlement Tx fail because account funds are insufficient. error:{0}")]
	InsufficientFunds(String),
	#[error("MCR Settlement Tx send fail because :{0}")]
	SendTxError(#[from] alloy_contract::Error),
	#[error("MCR Settlement Tx send fail during its execution :{0}")]
	RpcTxExecution(String),
	#[error("MCR Settlement BlockAccepted event notification error :{0}")]
	EventNotificationError(#[from] alloy_sol_types::Error),
	#[error("MCR Settlement BlockAccepted event notification stream close")]
	EventNotificationStreamClosed,
}

// Codegen from artifact.
sol!(
	#[allow(missing_docs)]
	#[sol(rpc)]
	MCR,
	"abi/MCR.json"
);

pub struct McrEthSettlementClient<P, T> {
	rpc_provider: P,
	signer_address: Address,
	ws_provider: RootProvider<PubSubFrontend>,
	config: McrEthSettlementConfig,
	_markert: PhantomData<T>,
}

impl
	McrEthSettlementClient<
		FillProvider<
			JoinFill<
				JoinFill<
					JoinFill<JoinFill<alloy_provider::Identity, GasFiller>, NonceFiller>,
					ChainIdFiller,
				>,
				SignerFiller<EthereumSigner>,
			>,
			RootProvider<BoxTransport>,
			BoxTransport,
			Ethereum,
		>,
		BoxTransport,
	>
{
	pub async fn build_with_urls<S2>(
		rpc: &str,
		ws_url: S2,
		signer_private_key: &str,
		config: McrEthSettlementConfig,
	) -> Result<Self, anyhow::Error>
	where
		S2: Into<String>,
	{
		let signer: LocalWallet = signer_private_key.parse()?;
		let signer_address = signer.address();
		let rpc_provider = ProviderBuilder::new()
			.with_recommended_fillers()
			.signer(EthereumSigner::from(signer))
			.on_builtin(rpc)
			.await?;

		McrEthSettlementClient::build_with_provider(rpc_provider, signer_address, ws_url, config)
			.await
	}
}

impl<P: Provider<T, Ethereum> + Clone, T: Transport + Clone> McrEthSettlementClient<P, T> {
	pub async fn build_with_provider<S>(
		rpc_provider: P,
		signer_address: Address,
		ws_url: S,
		config: McrEthSettlementConfig,
	) -> Result<Self, anyhow::Error>
	where
		S: Into<String>,
	{
		let ws = WsConnect::new(ws_url);

		let ws_provider = ProviderBuilder::new().on_ws(ws).await?;

		Ok(McrEthSettlementClient {
			rpc_provider,
			signer_address,
			ws_provider,
			config,
			_markert: Default::default(),
		})
	}

	async fn send_tx<D: CallDecoder + Clone>(
		&self,
		base_call_builder: CallBuilder<T, &&P, D, Ethereum>,
	) -> Result<(), anyhow::Error> {
		//validate gaz price.
		let mut estimate_gas = base_call_builder.estimate_gas().await?;
		// Add 20% because initial gas estimate are too low.
		estimate_gas += (estimate_gas * 20) / 100;

		// Sending Tx automatically can lead to errors that depend on the state for Eth.
		// It's convenient to manage some of them automatically to avoid to fail commitment Tx.
		// I define a first one but other should be added depending on the test with mainnet.
		for _ in 0..self.config.tx_send_nb_retry {
			let call_builder = base_call_builder.clone().gas(estimate_gas);

			//detect if the gas price doesn't execeed the limit.
			let gas_price = call_builder.provider.get_gas_price().await?;
			let tx_fee_wei = estimate_gas * gas_price;
			if tx_fee_wei > self.config.gas_limit {
				return Err(McrEthConnectorError::GasLimitExceed(
					tx_fee_wei,
					self.config.gas_limit,
				)
				.into());
			}

			//send the Tx and detect send error.
			let pending_tx = match call_builder.send().await {
				Err(alloy_contract::Error::TransportError(TransportError::ErrorResp(payload))) => {
					match payload.code {
						//transaction underpriced
						-32000 => {
							if payload.message.contains("transaction underpriced") {
								//increase gas of 10% and retry
								estimate_gas += (estimate_gas * 10) / 100;
								tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
								continue;
							} else if payload.message.contains("insufficient funds") {
								return Err(McrEthConnectorError::InsufficientFunds(
									payload.message,
								)
								.into());
							}
						},
						_ => (),
					}
					return Err(McrEthConnectorError::from(alloy_contract::Error::TransportError(
						TransportError::ErrorResp(payload),
					))
					.into());
				},
				Ok(pending_tx) => pending_tx,
				Err(err) => return Err(McrEthConnectorError::from(err).into()),
			};

			match pending_tx.get_receipt().await {
				// Tx execution fail
				Ok(tx_receipt) if !tx_receipt.status() => {
					tracing::debug!(
						"tx_receipt.gas_used: {} / estimate_gas: {estimate_gas}",
						tx_receipt.gas_used
					);
					if tx_receipt.gas_used == estimate_gas {
						tracing::warn!("Send commitment Tx  fail because of insufficient gas, receipt:{tx_receipt:?} ");
						estimate_gas += (estimate_gas * 10) / 100;
						continue;
					} else {
						return Err(McrEthConnectorError::RpcTxExecution(format!(
							"Send commitment Tx fail, abort Tx, receipt:{tx_receipt:?}"
						))
						.into());
					}
				},
				Ok(_) => return Ok(()),
				Err(err) => {
					return Err(McrEthConnectorError::RpcTxExecution(err.to_string()).into())
				},
			};
		}

		//Max retry exceed
		Err(McrEthConnectorError::RpcTxExecution(
			"Send commitment Tx fail because of exceed max retry".to_string(),
		)
		.into())
	}
}

#[async_trait::async_trait]
impl<P: Provider<T, Ethereum> + Clone, T: Transport + Clone> McrSettlementClientOperations
	for McrEthSettlementClient<P, T>
{
	async fn post_block_commitment(
		&self,
		block_commitment: BlockCommitment,
	) -> Result<(), anyhow::Error> {
		let contract =
			MCR::new(self.config.mrc_contract_address.parse().unwrap(), &self.rpc_provider);

		let eth_block_commitment = MCR::BlockCommitment {
			// currently, to simplify the api, we'll say 0 is uncommitted all other numbers are legitimate heights
			height: U256::from(block_commitment.height),
			commitment: alloy_primitives::FixedBytes(block_commitment.commitment.0),
			blockId: alloy_primitives::FixedBytes(block_commitment.block_id.0),
		};

		let call_builder = contract.submitBlockCommitment(eth_block_commitment);

		self.send_tx(call_builder).await
	}

	async fn post_block_commitment_batch(
		&self,
		block_commitments: Vec<BlockCommitment>,
	) -> Result<(), anyhow::Error> {
		let contract =
			MCR::new(self.config.mrc_contract_address.parse().unwrap(), &self.rpc_provider);

		let eth_block_commitment: Vec<_> = block_commitments
			.into_iter()
			.map(|block_commitment| {
				Ok(MCR::BlockCommitment {
					// currently, to simplify the api, we'll say 0 is uncommitted all other numbers are legitimate heights
					height: U256::from(block_commitment.height),
					commitment: alloy_primitives::FixedBytes(block_commitment.commitment.0),
					blockId: alloy_primitives::FixedBytes(block_commitment.block_id.0),
				})
			})
			.collect::<Result<Vec<_>, TryFromSliceError>>()?;

		let call_builder = contract.submitBatchBlockCommitment(eth_block_commitment);

		self.send_tx(call_builder).await
	}

	async fn stream_block_commitments(&self) -> Result<CommitmentStream, anyhow::Error> {
		//register to contract BlockCommitmentSubmitted event

		let contract =
			MCR::new(self.config.mrc_contract_address.parse().unwrap(), &self.ws_provider);
		let event_filter = contract.BlockAccepted_filter().watch().await?;

		let stream = event_filter.into_stream().map(|event| {
			event
				.map(|(commitment, _)| AcceptedBlockCommitment {
					height: commitment.height.try_into().unwrap(),
					block_id: Id(commitment.blockHash.0),
					commitment: Commitment(commitment.stateCommitment.0),
				})
				.map_err(|err| McrEthConnectorError::EventNotificationError(err).into())
		});
		Ok(Box::pin(stream) as CommitmentStream)
	}

	async fn get_commitment_at_height(
		&self,
		height: u64,
	) -> Result<Option<BlockCommitment>, anyhow::Error> {
		let contract =
			MCR::new(self.config.mrc_contract_address.parse().unwrap(), &self.ws_provider);
		let MCR::getValidatorCommitmentAtBlockHeightReturn { _0: commitment } = contract
			.getValidatorCommitmentAtBlockHeight(U256::from(height), self.signer_address)
			.call()
			.await?;
		let return_height: u64 = commitment.height.try_into()?;
		// Commitment with height 0 mean not found
		Ok((return_height != 0).then_some(BlockCommitment {
			height: commitment.height.try_into()?,
			block_id: Id(commitment.blockId.into()),
			commitment: Commitment(commitment.commitment.into()),
		}))
	}

	async fn get_max_tolerable_block_height(&self) -> Result<u64, anyhow::Error> {
		let contract =
			MCR::new(self.config.mrc_contract_address.parse().unwrap(), &self.ws_provider);
		let MCR::getMaxTolerableBlockHeightReturn { _0: block_height } =
			contract.getMaxTolerableBlockHeight().call().await?;
		let return_height: u64 = block_height.try_into()?;
		Ok(return_height)
	}
}

#[cfg(test)]
pub mod test {
	use super::*;
	use alloy_provider::ProviderBuilder;
	use alloy_signer_wallet::LocalWallet;
	use movement_types::Commitment;
	use std::env;

	//define 2 validator (signer1 and signer2) with each 50% of stake.
	// After after genesis ceremonial, 2 validator send the commitment for height 1.
	// Validator2 send a commitment for height 2 to trigger next epoch and fire event.
	// Wait the commitment accepted event.
	//#[ignore]
	#[tokio::test]
	async fn test_send_commitment() -> Result<(), anyhow::Error> {
		//Activate to debug the test.
		// use tracing_subscriber::EnvFilter;

		// tracing_subscriber::fmt()
		// 	.with_env_filter(
		// 		EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
		// 	)
		// 	.init();

		// Inititalize Test variables
		let rpc_port = env::var("MCR_ANVIL_PORT").unwrap();
		let rpc_url = format!("http://localhost:{rpc_port}");
		let ws_url = format!("ws://localhost:{rpc_port}");

		let anvil_address = read_anvil_json_file_address()?;

		//Do SC ceremony init stake calls.
		do_genesis_ceremonial(&anvil_address, &rpc_url).await?;

		let mcr_address = read_mcr_sc_adress()?;
		//Define Signers. Ceremony define 2 signers with half stake each.
		let signer1: LocalWallet = anvil_address[1].1.parse()?;
		let signer1_addr = signer1.address();

		//Build client 1 and send first commitment.
		let provider_client1 = ProviderBuilder::new()
			.with_recommended_fillers()
			.signer(EthereumSigner::from(signer1))
			.on_http(rpc_url.parse().unwrap());

		let config = McrEthSettlementConfig {
			mrc_contract_address: mcr_address.to_string(),
			gas_limit: DEFAULT_TX_GAS_LIMIT,
			tx_send_nb_retry: MAX_TX_SEND_RETRY,
		};

		let client1 = McrEthSettlementClient::build_with_provider(
			provider_client1,
			signer1_addr,
			ws_url.clone(),
			config.clone(),
		)
		.await
		.unwrap();

		let mut client1_stream = client1.stream_block_commitments().await.unwrap();

		//client post a new commitment
		let commitment =
			BlockCommitment { height: 1, block_id: Id([2; 32]), commitment: Commitment([3; 32]) };

		let res = client1.post_block_commitment(commitment.clone()).await;
		assert!(res.is_ok());

		//no notification quorum is not reach
		//TODO

		//Build client 2 and send the second commitment.
		let client2 =
			McrEthSettlementClient::build_with_urls(&rpc_url, ws_url, &anvil_address[2].1, config)
				.await
				.unwrap();

		let mut client2_stream = client2.stream_block_commitments().await.unwrap();

		//client post a new commitment
		let res = client2.post_block_commitment(commitment).await;
		assert!(res.is_ok());

		// now we move to block 2 and make some commitment just to trigger the epochRollover
		let commitment2 =
			BlockCommitment { height: 2, block_id: Id([4; 32]), commitment: Commitment([5; 32]) };

		let res = client2.post_block_commitment(commitment2.clone()).await;
		assert!(res.is_ok());

		//validate that the accept commitment stream get the event.
		let event = client1_stream.next().await.unwrap().unwrap();
		assert_eq!(event.commitment.0[0], 3);
		assert_eq!(event.block_id.0[0], 2);
		let event = client2_stream.next().await.unwrap().unwrap();
		assert_eq!(event.commitment.0[0], 3);
		assert_eq!(event.block_id.0[0], 2);

		//test post batch commitment
		// post the complementary batch on height 2 and one on height 3
		let commitment3 =
			BlockCommitment { height: 3, block_id: Id([6; 32]), commitment: Commitment([7; 32]) };
		let res = client1.post_block_commitment_batch(vec![commitment2, commitment3]).await;
		assert!(res.is_ok());
		//validate that the accept commitment stream get the event.
		let event = client1_stream.next().await.unwrap().unwrap();
		assert_eq!(event.commitment.0[0], 5);
		assert_eq!(event.block_id.0[0], 4);
		let event = client2_stream.next().await.unwrap().unwrap();
		assert_eq!(event.commitment.0[0], 5);
		assert_eq!(event.block_id.0[0], 4);

		//test get_commitment_at_height
		let commitment = client1.get_commitment_at_height(1).await?;
		assert!(commitment.is_some());
		let commitment = commitment.unwrap();
		assert_eq!(commitment.commitment.0[0], 3);
		assert_eq!(commitment.block_id.0[0], 2);
		let commitment = client1.get_commitment_at_height(10).await?;
		assert_eq!(commitment, None);

		Ok(())
	}

	use serde_json::{from_str, Value};
	use std::fs;
	fn read_anvil_json_file_address() -> Result<Vec<(String, String)>, anyhow::Error> {
		let anvil_conf_file = env::var("ANVIL_JSON_PATH")?;
		let file_content = fs::read_to_string(anvil_conf_file)?;

		let json_value: Value = from_str(&file_content)?;

		// Extract the available_accounts and private_keys fields
		let available_accounts_iter = json_value["available_accounts"]
			.as_array()
			.expect("available_accounts should be an array")
			.iter()
			.map(|v| v.as_str().map(|s| s.to_string()))
			.flatten();

		let private_keys_iter = json_value["private_keys"]
			.as_array()
			.expect("private_keys should be an array")
			.iter()
			.map(|v| v.as_str().map(|s| s.to_string()))
			.flatten();

		let res = available_accounts_iter
			.zip(private_keys_iter)
			.collect::<Vec<(String, String)>>();
		Ok(res)
	}

	fn read_mcr_sc_adress() -> Result<Address, anyhow::Error> {
		let file_path = env::var("MCR_SC_ADDRESS_FILE")?;
		let addr_str = fs::read_to_string(file_path)?;
		let addr: Address = addr_str.trim().parse()?;
		Ok(addr)
	}

	// Do the Genesis ceremony in Rust because if node by forge script,
	// it's never done from Rust call.
	use alloy_primitives::Bytes;
	use alloy_rpc_types::TransactionRequest;

	async fn do_genesis_ceremonial(
		anvil_address: &[(String, String)],
		rpc_url: &str,
	) -> Result<(), anyhow::Error> {
		let mcr_address = read_mcr_sc_adress()?;
		//Define Signer. Signer1 is the MCRSettelement client
		let signer1: LocalWallet = anvil_address[1].1.parse()?;
		let signer1_addr: Address = anvil_address[1].0.parse()?;
		let signer1_rpc_provider = ProviderBuilder::new()
			.with_recommended_fillers()
			.signer(EthereumSigner::from(signer1))
			.on_http(rpc_url.parse()?);
		let signer1_contract = MCR::new(mcr_address, &signer1_rpc_provider);

		let MCR::getGenesisStakeRequiredReturn { _0: get_genesis_stake_required } =
			signer1_contract.getGenesisStakeRequired().call().await?;
		let get_genesis_stake_required: u128 = get_genesis_stake_required.try_into().unwrap();
		stake_genesis(
			&signer1_rpc_provider,
			&signer1_contract,
			mcr_address,
			signer1_addr,
			55_000_000_000_000_000_000,
		)
		.await?;

		let signer2: LocalWallet = anvil_address[2].1.parse()?;
		let signer2_addr: Address = anvil_address[2].0.parse()?;
		let signer2_rpc_provider = ProviderBuilder::new()
			.with_recommended_fillers()
			.signer(EthereumSigner::from(signer2))
			.on_http(rpc_url.parse()?);
		let signer2_contract = MCR::new(mcr_address, &signer2_rpc_provider);

		//init staking
		// Build a transaction to set the values.
		stake_genesis(
			&signer2_rpc_provider,
			&signer2_contract,
			mcr_address,
			signer2_addr,
			54_000_000_000_000_000_000,
		)
		.await?;

		let MCR::hasGenesisCeremonyEndedReturn { _0: has_genesis_ceremony_ended } =
			signer2_contract.hasGenesisCeremonyEnded().call().await?;
		let ceremony: bool = has_genesis_ceremony_ended.try_into().unwrap();
		assert!(ceremony);
		Ok(())
	}

	async fn stake_genesis<P: Provider<T, Ethereum>, T: Transport + Clone>(
		provider: &P,
		contract: &MCR::MCRInstance<T, &P, Ethereum>,
		contract_address: Address,
		signer: Address,
		amount: u128,
	) -> Result<(), anyhow::Error> {
		let stake_genesis_call = contract.stakeGenesis();
		let calldata = stake_genesis_call.calldata().to_owned();
		sendtx_function(provider, calldata, contract_address, signer, amount).await
	}
	async fn sendtx_function<P: Provider<T, Ethereum>, T: Transport + Clone>(
		provider: &P,
		call_data: Bytes,
		contract_address: Address,
		signer: Address,
		amount: u128,
	) -> Result<(), anyhow::Error> {
		let eip1559_fees = provider.estimate_eip1559_fees(None).await?;
		let tx = TransactionRequest::default()
			.from(signer)
			.to(contract_address)
			.value(U256::from(amount))
			.input(call_data.into())
			.max_fee_per_gas(eip1559_fees.max_fee_per_gas)
			.max_priority_fee_per_gas(eip1559_fees.max_priority_fee_per_gas);

		provider.send_transaction(tx).await?.get_receipt().await?;
		Ok(())
	}
}
