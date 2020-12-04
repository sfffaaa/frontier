// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
// This file is part of Frontier.
//
// Copyright (c) 2020 Parity Technologies (UK) Ltd.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

mod eth;
mod eth_pubsub;

pub use eth::{EthApi, EthApiServer, NetApi, NetApiServer, Web3Api, Web3ApiServer};
pub use eth_pubsub::{EthPubSubApi, EthPubSubApiServer, HexEncodedIdProvider};

use ethereum_types::{H160, H256};
use jsonrpc_core::{ErrorCode, Error, Value};
use rustc_hex::ToHex;
use pallet_evm::ExitReason;
use sha3::{Digest, Keccak256};

mod estimate_gas_binary {
	use std::borrow::Cow;
	use ethereum_types::{H160, H256, U256};
	use pallet_evm::{
		BackendT, EvmBasic, Vicinity, Config, StackExecutor,
		Precompiles as PrecompilesT, ExitReason, ExitFatal,
	};
	use fc_rpc_core::types::{CallRequest, Bytes};
	use jsonrpc_core::Error;
	use crate::error_on_execution_failure;

	pub struct Backend<'vicinity> {
		_vicinity: &'vicinity Vicinity,
	}

	impl<'vicinity> Backend<'vicinity> {
		pub fn new(_vicinity: &'vicinity Vicinity) -> Self {
			Self { _vicinity }
		}
	}

	impl<'vicinity> BackendT for Backend<'vicinity> {
		fn gas_price(&self) -> U256 { U256::zero() }
		fn origin(&self) -> H160 { H160::default() }
		fn block_hash(&self, _number: U256) -> H256 { H256::default() }
		fn block_number(&self) -> U256 { U256::zero() }
		fn block_coinbase(&self) -> H160 { H160::default() }
		fn block_timestamp(&self) -> U256 { U256::zero() }
		fn block_difficulty(&self) -> U256 { U256::zero() }
		fn block_gas_limit(&self) -> U256 { U256::zero() }
		fn chain_id(&self) -> U256 { U256::zero() }
		fn exists(&self, _address: H160) -> bool { true }
		fn basic(&self, _address: H160) -> EvmBasic {
			EvmBasic {
				balance: U256::zero(),
				nonce: U256::zero(),
			}
		}
		fn code_size(&self, _address: H160) -> usize { 0 as usize }
		fn code_hash(&self, _address: H160) -> H256 { H256::default() }
		fn code(&self, _address: H160) -> Vec<u8> { Vec::new() }
		fn storage(&self, _address: H160, _index: H256) -> H256 { H256::default() }
	}

	pub fn execute(request: CallRequest) -> Result<U256, Error> {

		type Precompiles = (
			pallet_evm::precompiles::ECRecover,
			pallet_evm::precompiles::Sha256,
			pallet_evm::precompiles::Ripemd160,
			pallet_evm::precompiles::Identity,
		);

		let CallRequest {
			from,
			to,
			gas_price,
			gas,
			value,
			data,
			..
		} = request;

		let evm_config = Config::istanbul();

		let gas_limit = gas.unwrap_or(U256::max_value()); // TODO: set a limit

		let vicinity = Vicinity {
			gas_price: gas_price.unwrap_or_default(),
			origin: to.unwrap_or_default(),
		};

		let backend = Backend::new(&vicinity);

		let mut high: u32 = gas_limit.low_u32();
		let mut low: u32 = 21000;

		let mut current: u32;
		let mut exit_reason: Option<ExitReason> = None;
		while low + 1 < high {
			current = (high.saturating_add(low)) / 2;

			let mut executor = StackExecutor::new_with_precompile(
				&backend,
				current as usize,
				&evm_config,
				<Precompiles as PrecompilesT>::execute
			); 

			let reason = match to {
				Some(to) => {
					executor.transact_call(
						from.unwrap_or_default(),
						to,
						value.unwrap_or_default(),
						data.clone().unwrap_or(
							Bytes::new(Vec::new())
						).0,
						current as usize,
					).0
				},
				None => {
					executor.transact_create(
						from.unwrap_or_default(),
						value.unwrap_or_default(),
						data.clone().unwrap_or(
							Bytes::new(Vec::new())
						).0,
						current as usize,
					)
				}
			};
			if let ExitReason::Error(_) = reason {
				low = current;
				exit_reason = Some(reason);
				continue;
			}
			exit_reason = Some(reason);
			high = current;
		}

		if exit_reason.is_none() {
			exit_reason = Some(ExitReason::Fatal(
				ExitFatal::Other(Cow::from("Unknown exit reason"))
			));
		}
		error_on_execution_failure(&exit_reason.unwrap(), &[])?;
		Ok(U256::from(high))
	}
}

pub fn internal_err<T: ToString>(message: T) -> Error {
	Error {
		code: ErrorCode::InternalError,
		message: message.to_string(),
		data: None
	}
}

pub fn error_on_execution_failure(reason: &ExitReason, data: &[u8]) -> Result<(), Error> {
	match reason {
		ExitReason::Succeed(_) => Ok(()),
		ExitReason::Error(e) => {
			Err(Error {
				code: ErrorCode::InternalError,
				message: format!("evm error: {:?}", e),
				data: Some(Value::String("0x".to_string()))
			})
		},
		ExitReason::Revert(_) => {
			let mut message = "VM Exception while processing transaction: revert".to_string();
			// A minimum size of error function selector (4) + offset (32) + string length (32)
			// should contain a utf-8 encoded revert reason.
			if data.len() > 68 {
				let message_len = data[36..68].iter().sum::<u8>();
				let body: &[u8] = &data[68..68 + message_len as usize];
				if let Ok(reason) = std::str::from_utf8(body) {
					message = format!("{} {}", message, reason.to_string());
				}
			}
			Err(Error {
				code: ErrorCode::InternalError,
				message,
				data: Some(Value::String(data.to_hex()))
			})
		},
		ExitReason::Fatal(e) => {
			Err(Error {
				code: ErrorCode::InternalError,
				message: format!("evm fatal: {:?}", e),
				data: Some(Value::String("0x".to_string()))
			})
		},
	}
}

/// A generic Ethereum signer.
pub trait EthSigner: Send + Sync {
	/// Available accounts from this signer.
	fn accounts(&self) -> Vec<H160>;
	/// Sign a transaction message using the given account in message.
	fn sign(
		&self,
		message: ethereum::TransactionMessage,
		address: &H160,
	) -> Result<ethereum::Transaction, Error>;
}

pub struct EthDevSigner {
	keys: Vec<secp256k1::SecretKey>,
}

impl EthDevSigner {
	pub fn new() -> Self {
		Self {
			keys: vec![
				secp256k1::SecretKey::parse(&[
					0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
					0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
					0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
					0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
				]).expect("Test key is valid; qed"),
			],
		}
	}
}

impl EthSigner for EthDevSigner {
	fn accounts(&self) -> Vec<H160> {
		self.keys.iter().map(|secret| {
			let public = secp256k1::PublicKey::from_secret_key(secret);
			let mut res = [0u8; 64];
			res.copy_from_slice(&public.serialize()[1..65]);

			H160::from(H256::from_slice(Keccak256::digest(&res).as_slice()))
		}).collect()
	}

	fn sign(
		&self,
		message: ethereum::TransactionMessage,
		address: &H160,
	) -> Result<ethereum::Transaction, Error> {
		let mut transaction = None;

		for secret in &self.keys {
			let key_address = {
				let public = secp256k1::PublicKey::from_secret_key(secret);
				let mut res = [0u8; 64];
				res.copy_from_slice(&public.serialize()[1..65]);
				H160::from(H256::from_slice(Keccak256::digest(&res).as_slice()))
			};

			if &key_address == address {
				let signing_message = secp256k1::Message::parse_slice(&message.hash()[..])
					.map_err(|_| internal_err("invalid signing message"))?;
				let (signature, recid) = secp256k1::sign(&signing_message, secret);

				let v = match message.chain_id {
					None => 27 + recid.serialize() as u64,
					Some(chain_id) => 2 * chain_id + 35 + recid.serialize() as u64,
				};
				let rs = signature.serialize();
				let r = H256::from_slice(&rs[0..32]);
				let s = H256::from_slice(&rs[32..64]);

				transaction = Some(ethereum::Transaction {
					nonce: message.nonce,
					gas_price: message.gas_price,
					gas_limit: message.gas_limit,
					action: message.action,
					value: message.value,
					input: message.input.clone(),
					signature: ethereum::TransactionSignature::new(v, r, s)
						.ok_or(internal_err("signer generated invalid signature"))?,
				});

				break
			}
		}

		transaction.ok_or(internal_err("signer not available"))
	}
}
