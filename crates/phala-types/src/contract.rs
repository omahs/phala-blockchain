use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_core::{bounded::BoundedVec, ConstU32};

use crate::WorkerPublicKey;
pub use phala_mq::{ContractClusterId, ContractId};

pub type ContractId32 = u32;
pub const SYSTEM: ContractId32 = 0;
pub const DATA_PLAZA: ContractId32 = 1;
pub const BALANCES: ContractId32 = 2;
pub const ASSETS: ContractId32 = 3;
pub const WEB3_ANALYTICS: ContractId32 = 4;
pub const _DIEM: ContractId32 = 5;
pub const SUBSTRATE_KITTIES: ContractId32 = 6;
pub const BTC_LOTTERY: ContractId32 = 7;
pub const GEOLOCATION: ContractId32 = 8;
pub const GUESS_NUMBER: ContractId32 = 100;
pub const BTC_PRICE_BOT: ContractId32 = 101;

#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo)]
pub enum CodeIndex<CodeHash> {
    WasmCode(CodeHash),
}

#[derive(Decode, Encode, TypeInfo)]
pub enum InkCommand {
    InkMessage {
        nonce: BoundedVec<u8, ConstU32<32>>,
        message: Vec<u8>,
        // Amount of tokens transfer to the target contract
        transfer: u128,
        // Max value gas allowed to be consumed
        gas_limit: u64,
        // Max value token allowed to be deposited to the storage usage
        storage_deposit_limit: Option<u128>,
    },
}

impl<CodeHash: AsRef<[u8]>> CodeIndex<CodeHash> {
    pub fn code_hash(&self) -> Vec<u8> {
        match self {
            CodeIndex::WasmCode(code_hash) => code_hash.as_ref().to_vec(),
        }
    }
}

pub mod messaging {
    use alloc::{collections::BTreeMap, vec::Vec};
    use codec::{Decode, Encode};
    use core::fmt::Debug;
    use scale_info::TypeInfo;

    use super::{ContractClusterId, ContractInfo};
    use crate::messaging::EncryptedKey;
    use crate::{ClusterPublicKey, WorkerIdentity, WorkerPublicKey};
    use phala_mq::bind_topic;
    use sp_core::crypto::AccountId32;

    bind_topic!(ClusterEvent, b"phala/cluster/event");
    #[derive(Encode, Decode, Debug)]
    pub enum ClusterEvent {
        // TODO.shelven: enable add and remove workers
        DeployCluster {
            owner: AccountId32,
            cluster: ContractClusterId,
            workers: Vec<WorkerIdentity>,
            deposit: u128, // Amount of balance transfering from chain into the cluster for the owner
            gas_price: u128,
            deposit_per_item: u128,
            deposit_per_byte: u128,
            treasury_account: AccountId32,
        },
    }

    bind_topic!(ContractOperation<CodeHash, AccountId>, b"phala/contract/op");
    #[derive(Encode, Decode, Debug)]
    pub enum ContractOperation<CodeHash, AccountId> {
        InstantiateCode {
            contract_info: ContractInfo<CodeHash, AccountId>,
            transfer: u128,
            gas_limit: u64,
            storage_deposit_limit: Option<u128>,
        },
    }

    impl<CodeHash, AccountId> ContractOperation<CodeHash, AccountId> {
        pub fn instantiate_code(
            contract_info: ContractInfo<CodeHash, AccountId>,
            transfer: u128,
            gas_limit: u64,
            storage_deposit_limit: Option<u128>,
        ) -> Self {
            ContractOperation::InstantiateCode {
                contract_info,
                transfer,
                gas_limit,
                storage_deposit_limit,
            }
        }
    }

    // Pink messages
    #[derive(Encode, Decode, Debug, PartialEq, Eq, TypeInfo, Clone, Copy)]
    pub enum ResourceType {
        InkCode,
        SidevmCode,
        IndeterministicInkCode,
    }

    bind_topic!(WorkerClusterReport, b"phala/cluster/worker/report");
    #[derive(Encode, Decode, Debug, TypeInfo)]
    pub enum WorkerClusterReport {
        ClusterDeployed {
            id: ContractClusterId,
            pubkey: ClusterPublicKey,
        },
        ClusterDeploymentFailed {
            id: ContractClusterId,
        },
    }

    #[derive(Encode, Decode, TypeInfo, Clone, PartialEq, Eq, Debug)]
    pub struct BatchDispatchClusterKeyEvent {
        pub secret_keys: BTreeMap<WorkerPublicKey, EncryptedKey>,
        pub cluster: ContractClusterId,
        /// The owner of the cluster
        pub owner: AccountId32,
        pub deposit: u128,
        pub gas_price: u128,
        pub deposit_per_item: u128,
        pub deposit_per_byte: u128,
        pub treasury_account: AccountId32,
    }

    bind_topic!(ClusterOperation<AccountId>, b"phala/cluster/key");
    #[derive(Encode, Decode, Clone, Debug, TypeInfo)]
    pub enum ClusterOperation<AccountId> {
        // TODO.shelven: a better way for real large batch key distribution
        /// MessageOrigin::Gatekeeper -> ALL
        DispatchKeys(BatchDispatchClusterKeyEvent),
        /// Force destroying a cluster.
        ///
        /// This leaves a door to clean up the beta clusters in phat v1.
        /// We might need to redesign a more graceful one in the future.
        DestroyCluster(ContractClusterId),
        /// Upload ink code to the cluster.
        UploadResource {
            origin: AccountId,
            cluster_id: ContractClusterId,
            resource_type: ResourceType,
            resource_data: Vec<u8>,
        },
        Deposit {
            cluster_id: ContractClusterId,
            account: AccountId,
            amount: u128,
        },
        RemoveWorker {
            cluster_id: ContractClusterId,
            worker: WorkerPublicKey,
        },
    }

    impl<AccountId> ClusterOperation<AccountId> {
        #[allow(clippy::too_many_arguments)]
        pub fn batch_distribution(
            secret_keys: BTreeMap<WorkerPublicKey, EncryptedKey>,
            cluster: ContractClusterId,
            owner: AccountId32,
            deposit: u128,
            gas_price: u128,
            deposit_per_item: u128,
            deposit_per_byte: u128,
            treasury_account: AccountId32,
        ) -> Self {
            ClusterOperation::DispatchKeys(BatchDispatchClusterKeyEvent {
                secret_keys,
                cluster,
                owner,
                deposit,
                gas_price,
                deposit_per_item,
                deposit_per_byte,
                treasury_account,
            })
        }
    }
}

#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo)]
pub enum ClusterPermission<AccountId> {
    Public,
    OnlyOwner(AccountId),
}

#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo)]
pub struct ClusterInfo<AccountId> {
    pub owner: AccountId,
    pub permission: ClusterPermission<AccountId>,
    pub workers: Vec<WorkerPublicKey>,
    pub system_contract: ContractId,
    pub gas_price: u128,
    pub deposit_per_item: u128,
    pub deposit_per_byte: u128,
}

/// On-chain contract registration info
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo)]
pub struct ContractInfo<CodeHash, AccountId> {
    pub deployer: AccountId,
    pub code_index: CodeIndex<CodeHash>,
    pub salt: Vec<u8>,
    pub cluster_id: ContractClusterId,
    pub instantiate_data: Vec<u8>,
}

/// Use blake2_256 on the preimage for the final contract id
pub fn contract_id_preimage(
    deployer: &[u8],
    code_hash: &[u8],
    cluster_id: &[u8],
    salt: &[u8],
) -> Vec<u8> {
    let buf: Vec<_> = deployer
        .iter()
        .chain(code_hash)
        .chain(cluster_id)
        .chain(salt)
        .cloned()
        .collect();
    buf
}

impl<CodeHash: AsRef<[u8]>, AccountId: AsRef<[u8]>> ContractInfo<CodeHash, AccountId> {
    pub fn contract_id(&self, blake2_256: impl Fn(&[u8]) -> [u8; 32]) -> ContractId {
        let buf = contract_id_preimage(
            self.deployer.as_ref(),
            self.code_index.code_hash().as_ref(),
            self.cluster_id.as_ref(),
            self.salt.as_ref(),
        );
        ContractId::from(blake2_256(buf.as_ref()))
    }
}

/// Contract query request parameters, to be encrypted.
#[derive(Encode, Decode, Debug)]
pub struct ContractQuery<Data> {
    pub head: ContractQueryHead,
    /// The request data.
    pub data: Data,
}

/// Contract query head
#[derive(Encode, Decode, Debug)]
pub struct ContractQueryHead {
    /// The contract id.
    pub id: ContractId,
    /// A random byte array generated by the client.
    pub nonce: [u8; 32],
}

/// Contract query response, to be encrypted.
#[derive(Encode, Decode, Debug)]
pub struct ContractQueryResponse<Data> {
    /// The nonce from the client.
    pub nonce: [u8; 32],
    /// The query result.
    pub result: Data,
}

pub struct Data(pub Vec<u8>);

impl Encode for Data {
    fn size_hint(&self) -> usize {
        self.0.len()
    }
    fn encode_to<T: codec::Output + ?Sized>(&self, dest: &mut T) {
        dest.write(&self.0)
    }
}

/// Contract query error define
#[derive(Encode, Decode, Debug)]
pub enum ContractQueryError {
    /// Signature is invalid.
    InvalidSignature,
    /// No such contract.
    ContractNotFound,
    /// Unable to decode the request data.
    DecodeError,
    /// Other errors reported during the contract query execution.
    OtherError(String),
}

impl From<ContractQueryError> for prpc::server::Error {
    fn from(err: ContractQueryError) -> Self {
        Self::ContractQueryError(alloc::format!("{err:?}"))
    }
}

pub fn command_topic(id: ContractId) -> Vec<u8> {
    format!("phala/contract/{}/command", hex::encode(id))
        .as_bytes()
        .to_vec()
}

pub trait ConvertTo<To> {
    fn convert_to(&self) -> To;
}

impl<F, T> ConvertTo<T> for F
where
    F: AsRef<[u8; 32]>,
    T: From<[u8; 32]>,
{
    fn convert_to(&self) -> T {
        (*self.as_ref()).into()
    }
}
