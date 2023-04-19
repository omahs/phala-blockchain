use crate::api::TxStatusResponse;
use crate::datasource::WrappedDataSourceManager;
pub use crate::khala;
use crate::khala::runtime_types;
use crate::khala::runtime_types::khala_parachain_runtime::RuntimeCall;
use crate::khala::runtime_types::phala_pallets::utils::attestation_legacy;
use crate::khala::runtime_types::sp_runtime::DispatchError;
use crate::khala::utility::events::ItemFailed;
use crate::tx::TxManagerError::*;
use crate::use_parachain_api;
use anyhow::{anyhow, Error, Result};
use chrono::{DateTime, Utc};
use crossbeam::atomic::AtomicCell;
use crossbeam::queue::SegQueue;
use futures::future::{join_all, BoxFuture};
use hex::ToHex;
use lazy_static::lazy_static;
use log::{debug, error};
use moka_cht::HashMap;
use parity_scale_codec::{Decode, Encode};
use phala_types::messaging::SignedMessage;
use rocksdb::{DBCompactionStyle, DBWithThreadMode, MultiThreaded, Options};
use schnorrkel::keys::Keypair;
use serde::{Deserialize, Serialize};
use sp_core::crypto::{AccountId32, Ss58AddressFormat, Ss58Codec};
use sp_core::sr25519::{Pair as Sr25519Pair, Public as Sr25519Public};
use sp_core::Pair;
use std::collections::{HashMap as StdHashMap, VecDeque};
use std::fmt::{Debug, Display, Formatter};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use subxt::error::DispatchError as SubxtDispatchError;
use subxt::tx::PairSigner;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::{Stream, StreamExt};

static PHALA_SS58_FORMAT_U8: u8 = 30;

lazy_static! {
    static ref PHALA_SS58_FORMAT: Ss58AddressFormat = Ss58AddressFormat::from(PHALA_SS58_FORMAT_U8);
}

static TX_QUEUE_CHUNK_SIZE: usize = 30;
static TX_QUEUE_CHUNK_TIMEOUT_IN_MS: u64 = 1000;
static TX_TIMEOUT: u64 = 30000;

static PO_LIST: &str = "po_list";
static PO_BY_PID: &str = "po:pid:";

pub type DB = DBWithThreadMode<MultiThreaded>;

pub fn get_options(max_open_files: Option<i32>) -> Options {
    // Current tuning based off of the total ordered example, flash
    // storage example on
    // https://github.com/facebook/rocksdb/wiki/RocksDB-Tuning-Guide
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_compaction_style(DBCompactionStyle::Level);
    opts.set_write_buffer_size(67_108_864); // 64mb
    opts.set_max_write_buffer_number(3);
    opts.set_target_file_size_base(67_108_864); // 64mb
    opts.set_level_zero_file_num_compaction_trigger(8);
    opts.set_level_zero_slowdown_writes_trigger(17);
    opts.set_level_zero_stop_writes_trigger(24);
    opts.set_num_levels(4);
    opts.set_max_bytes_for_level_base(536_870_912); // 512mb
    opts.set_max_bytes_for_level_multiplier(8.0);

    if let Some(max_open_files) = max_open_files {
        opts.set_max_open_files(max_open_files);
    }

    opts
}

#[derive(Serialize, Deserialize, Clone)]
pub enum TransactionState {
    Pending,
    Running,
    Success(TransactionSuccess),
    Error(TransactionErrorMessage),
}

#[derive(thiserror::Error, Clone, Debug, Serialize)]
pub enum TxManagerError {
    #[error("Unknown data mismatch, this is a bug.")]
    UnknownDataMismatch,

    #[error("Operator of pool #{0} not set")]
    PoolOperatorNotSet(u64),

    #[error("There is no valid substrate data source")]
    NoValidSubstrateDataSource,

    #[error("Invalid pool operator")]
    InvalidPoolOperator,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TransactionSuccess {
    pub updated_at: DateTime<Utc>,
}

impl Default for TransactionSuccess {
    fn default() -> Self {
        Self {
            updated_at: Utc::now(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TransactionErrorMessage {
    pub updated_at: DateTime<Utc>,
    pub message: String,
}

impl From<&Error> for TransactionErrorMessage {
    fn from(e: &Error) -> Self {
        Self {
            updated_at: Utc::now(),
            message: e.to_string(),
        }
    }
}

impl Debug for Transaction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_string(self) {
            Ok(r) => write!(f, "{r}"),
            Err(e) => {
                panic!("{:?}", &e);
            }
        }
    }
}

impl Display for Transaction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_string(self) {
            Ok(r) => write!(f, "{r}"),
            Err(e) => {
                panic!("{:?}", &e);
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Transaction {
    pub id: usize,
    pub state: TransactionState,
    pub desc: String,
    pub pid: u64,
    pub created_at: DateTime<Utc>,
    #[serde(skip)]
    pub tx_payload: AtomicCell<Option<RuntimeCall>>,
    #[serde(skip)]
    pub shot: AtomicCell<Option<oneshot::Sender<Result<()>>>>,
}

impl Transaction {
    pub fn new(
        id: usize,
        pid: u64,
        tx_payload: RuntimeCall,
        desc: String,
        shot: oneshot::Sender<Result<()>>,
    ) -> Self {
        Self {
            id,
            state: TransactionState::Pending,
            desc,
            pid,
            created_at: Utc::now(),
            tx_payload: AtomicCell::new(Some(tx_payload)),
            shot: AtomicCell::new(Some(shot)),
        }
    }
    pub fn clone_for_serialize(&self) -> Self {
        Self {
            id: self.id,
            state: self.state.clone(),
            desc: self.desc.clone(),
            pid: self.pid,
            created_at: self.created_at,
            tx_payload: AtomicCell::new(None),
            shot: AtomicCell::new(None),
        }
    }
}

impl Clone for Transaction {
    fn clone(&self) -> Self {
        self.clone_for_serialize()
    }
}

pub struct TxQueueStream<T> {
    queue: SegQueue<T>,
}

impl<T> TxQueueStream<T> {
    pub fn push(&self, i: T) {
        self.queue.push(i)
    }
}

impl<T: Send> Stream for TxQueueStream<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.queue.pop() {
            Some(val) => Poll::Ready(Some(val)),
            None => Poll::Pending,
        }
    }
}

pub struct TxManager {
    pub db: Arc<DB>,
    dsm: WrappedDataSourceManager,
    tx_count: AtomicUsize,
    tx_map: HashMap<usize, Arc<Mutex<Transaction>>>,
    pending_txs: Mutex<VecDeque<usize>>,
    running_txs: Mutex<Vec<usize>>,
    past_txs: Mutex<VecDeque<usize>>,
    channel_tx: mpsc::UnboundedSender<usize>,
}

impl TxManager {
    pub async fn dump(self: Arc<Self>) -> Result<TxStatusResponse> {
        let tx_count = self.tx_count.load(Ordering::Relaxed);

        let pending_txs = self.pending_txs.lock().await;
        let pending_txs = pending_txs.clone();

        let running_txs = self.running_txs.lock().await;
        let running_txs = running_txs.clone();

        let past_txs = self.past_txs.lock().await;
        let past_txs = past_txs.clone();

        macro_rules! dump_tx_group {
            ($v: ident) => {{
                let mut r = Vec::new();
                for id in $v {
                    let tx = self.tx_map.get(&id).ok_or(UnknownDataMismatch)?;
                    let tx = tx.lock().await;
                    r.push(tx.clone_for_serialize())
                }
                r
            }};
        }

        let pending_txs = dump_tx_group!(pending_txs);
        let running_txs = dump_tx_group!(running_txs);
        let past_txs = dump_tx_group!(past_txs);

        Ok(TxStatusResponse {
            tx_count,
            running_txs,
            pending_txs,
            past_txs,
        })
    }
}

impl TxManager {
    pub fn new(
        path_base: &str,
        dsm: WrappedDataSourceManager,
    ) -> Result<(Arc<Self>, BoxFuture<'static, Result<()>>)> {
        let opts = get_options(None);
        let path = Path::new(path_base).join("po");
        let db = DB::open(&opts, path)?;

        let (tx, rx) = mpsc::unbounded_channel::<usize>();

        let txm = Arc::new(TxManager {
            db: Arc::new(db),
            dsm,
            tx_count: AtomicUsize::new(0),
            tx_map: HashMap::new(),
            pending_txs: Mutex::new(VecDeque::new()),
            running_txs: Mutex::new(Vec::new()),
            past_txs: Mutex::new(VecDeque::new()),
            channel_tx: tx,
        });
        let handle = Box::pin(txm.clone().start_trader(rx));

        Ok((txm, handle))
    }
    async fn start_trader(self: Arc<Self>, rx: mpsc::UnboundedReceiver<usize>) -> Result<()> {
        let rx_stream = UnboundedReceiverStream::new(rx).chunks_timeout(
            TX_QUEUE_CHUNK_SIZE,
            Duration::from_millis(TX_QUEUE_CHUNK_TIMEOUT_IN_MS),
        );
        tokio::pin!(rx_stream);

        while let Some(current_txs) = rx_stream.next().await {
            let mut pending_txs = self.pending_txs.lock().await;
            let mut running_txs = self.running_txs.lock().await;
            let mut past_txs = self.past_txs.lock().await;

            let last_running_txs = std::mem::take(&mut *running_txs);

            for _ in current_txs.iter() {
                let _ = pending_txs.pop_front();
            }
            for i in last_running_txs {
                past_txs.push_front(i);
            }

            drop(past_txs);
            drop(running_txs);
            drop(pending_txs);

            let mut tx_map: StdHashMap<u64, Vec<usize>> = StdHashMap::new();
            for i in current_txs {
                let tx = self.tx_map.get(&i).ok_or(UnknownDataMismatch)?;
                let pid = tx.lock().await.pid;
                if let Some(group) = tx_map.get_mut(&pid) {
                    group.push(i);
                } else {
                    let group = vec![i];
                    let _ = tx_map.insert(pid, group);
                };
            }
            join_all(
                tx_map
                    .into_iter()
                    .map(|(pid, v)| {
                        let self_move = self.clone();
                        async move {
                            if let Err(e) = self_move.clone().wrap_send_tx_group(pid, v).await {
                                error!("wrap_send_tx_group: {e}");
                                std::process::exit(255);
                            }
                        }
                    })
                    .collect::<Vec<_>>(),
            )
            .await;

            let mut running_txs = self.running_txs.lock().await;
            let mut past_txs = self.past_txs.lock().await;

            let last_running_txs = running_txs.clone();
            *running_txs = Vec::new();
            for i in last_running_txs {
                past_txs.push_front(i);
            }
            drop(running_txs);
            drop(past_txs);
        }
        error!("Unexpected exit of start_trader!");
        std::process::exit(255);
    }
    async fn wrap_send_tx_group(self: Arc<Self>, pid: u64, ids: Vec<usize>) -> Result<()> {
        if ids.is_empty() {
            anyhow::bail!("TxGroup can't be empty!");
        }

        for id in ids.clone() {
            let tx = self.tx_map.get(&id).ok_or(UnknownDataMismatch)?;
            let mut tx = tx.lock().await;
            tx.state = TransactionState::Running;
            drop(tx);
        }

        match self.clone().send_tx_group(pid, ids.clone()).await {
            Ok(ret) => {
                for (idx, r) in ret.into_iter().enumerate() {
                    let id = ids.get(idx).ok_or(UnknownDataMismatch)?;
                    let tx = self.clone().tx_map.get(id).ok_or(UnknownDataMismatch)?;
                    let mut tx = tx.lock().await;
                    let shot = tx.shot.swap(None).ok_or(UnknownDataMismatch)?;
                    tx.state = match &r {
                        Ok(_) => TransactionState::Success(TransactionSuccess::default()),
                        Err(e) => TransactionState::Error(e.into()),
                    };
                    if shot.send(r).is_err() {
                        return Err(anyhow!("shot can't be sent"));
                    }
                    drop(tx);
                }
            }
            Err(e) => {
                error!("send_tx_group: {}", &e);
                for id in ids {
                    let tx = self.clone().tx_map.get(&id).ok_or(UnknownDataMismatch)?;
                    let mut tx = tx.lock().await;
                    let shot = tx.shot.swap(None).ok_or(UnknownDataMismatch)?;
                    tx.state = TransactionState::Error((&e).into());
                    if shot.send(Err(anyhow!(e.to_string()))).is_err() {
                        return Err(anyhow!("shot can't be sent"));
                    }
                    drop(tx);
                }
            }
        }
        Ok(())
    }
    async fn send_tx_group(self: Arc<Self>, pid: u64, ids: Vec<usize>) -> Result<Vec<Result<()>>> {
        debug!("send_tx_group: {:?}", &ids);
        let po = self.db.get_po(pid)?.ok_or(InvalidPoolOperator)?;
        let proxied = po.proxied.is_some();

        let api = use_parachain_api!(self.dsm, false).ok_or(NoValidSubstrateDataSource)?;
        let mut calls = Vec::new();
        for i in ids.iter() {
            let tx = self.tx_map.get(i).ok_or(UnknownDataMismatch)?;
            let tx = tx.lock().await;
            let call = tx.tx_payload.swap(None).ok_or(UnknownDataMismatch)?;
            calls.push(call);
            drop(tx);
        }
        let signer = PairSigner::new(po.pair.clone());
        let tx = if proxied {
            let call =
                RuntimeCall::Utility(runtime_types::pallet_utility::pallet::Call::force_batch {
                    calls,
                });
            let call = khala::tx()
                .proxy()
                .proxy(po.proxied.unwrap().into(), None, call)
                .unvalidated();
            api.tx()
                .sign_and_submit_then_watch_default(&call, &signer)
                .await?
        } else {
            let call = khala::tx().utility().force_batch(calls).unvalidated();
            api.tx()
                .sign_and_submit_then_watch_default(&call, &signer)
                .await?
        };

        let tx = tokio::select! {
            t = tx.wait_for_in_block() => {
                Some(t?)
            }
            _ = tokio::time::sleep(Duration::from_millis(TX_TIMEOUT)) => {
                None
            }
        };

        let tx = if let Some(tx) = tx {
            tx
        } else {
            anyhow::bail!("Tx timed out!");
        };
        let tx = tx.wait_for_success().await?;

        if proxied {
            let event_proxy = tx
                .find_first::<khala::proxy::events::ProxyExecuted>()?
                .ok_or(anyhow!("ProxyExecuted event not found!"))?;
            if let Err(e) = event_proxy.result {
                anyhow::bail!("{:?}", &e);
            }
        }
        if tx
            .find_first::<khala::utility::events::BatchCompleted>()?
            .is_some()
        {
            return Ok((0..ids.len()).map(|_| Ok(())).collect::<Vec<_>>());
        }
        tx.find_first::<khala::utility::events::BatchCompletedWithErrors>()?
            .ok_or(anyhow!("BatchCompletedWithErrors event not found!"))?;

        let metadata = api.metadata();
        let mut ret = Vec::new();
        for i in tx.iter() {
            let i = i?;
            if i.pallet_name() == "Utility" {
                match i.variant_name() {
                    "ItemCompleted" => {
                        ret.push(Ok(()));
                    }
                    "ItemFailed" => {
                        let i = i
                            .as_event::<ItemFailed>()?
                            .ok_or(anyhow!("ItemFailed not parsed from event"))?;
                        let i = i.error;
                        let i_bytes = i.encode();
                        match i {
                            DispatchError::Module(_) => {
                                // current code works with subxt-0.27.1, should be updated after subxt upgraded to > 0.28
                                let e = SubxtDispatchError::decode_from(i_bytes, &metadata);
                                match e {
                                    SubxtDispatchError::Module(e) => ret.push(Err(anyhow!(
                                        format!("{}", e.description.join("\n"))
                                    ))),
                                    SubxtDispatchError::Other(_) => {
                                        ret.push(Err(anyhow!(format!("Error resolve failed"))))
                                    }
                                }
                            }
                            _ => {
                                ret.push(Err(anyhow!(format!("{:?}", &i))));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if ret.len() != ids.len() {
            anyhow::bail!("ItemCompleted or ItemFailed events incomplete!");
        }
        Ok(ret)
    }

    pub async fn send_to_queue(
        &self,
        pid: u64,
        tx_payload: RuntimeCall,
        desc: String,
    ) -> Result<()> {
        let (shot, rx) = oneshot::channel();
        tokio::pin!(rx);
        let mut gid = self.tx_count.load(Ordering::SeqCst);
        let id = gid;
        gid += 1;
        self.tx_count.store(gid, Ordering::SeqCst);
        debug!("send_to_queue: {:?}", &id);

        let mut pending_txs = self.pending_txs.lock().await;
        pending_txs.push_back(id);
        drop(pending_txs);

        self.tx_map.insert(
            id,
            Arc::new(Mutex::new(Transaction::new(
                id, pid, tx_payload, desc, shot,
            ))),
        );
        self.channel_tx.clone().send(id)?;
        rx.await?
    }
}

impl TxManager {
    pub async fn register_worker(
        self: Arc<Self>,
        pid: u64,
        pruntime_info: Vec<u8>,
        attestation: Vec<u8>,
        v2: bool,
    ) -> Result<()> {
        let tx_payload = if v2 {
            let mut pruntime_info = &pruntime_info[..];
            let pruntime_info =
                runtime_types::phala_types::WorkerRegistrationInfoV2::decode(&mut pruntime_info)?;
            let mut attestation = &attestation[..];
            let attestation = Option::decode(&mut attestation)?;
            RuntimeCall::PhalaRegistry(
                khala::runtime_types::phala_pallets::registry::pallet::Call::register_worker_v2 {
                    pruntime_info,
                    attestation,
                },
            )
        } else {
            let mut pruntime_info = &pruntime_info[..];
            let pruntime_info =
                runtime_types::phala_types::WorkerRegistrationInfo::decode(&mut pruntime_info)?;
            let mut attestation = &attestation[..];
            let attestation = attestation_legacy::Attestation::decode(&mut attestation)?;
            RuntimeCall::PhalaRegistry(
                khala::runtime_types::phala_pallets::registry::pallet::Call::register_worker {
                    pruntime_info,
                    attestation,
                },
            )
        };

        let desc = format!("Register worker for pool #{pid}");
        self.clone().send_to_queue(pid, tx_payload, desc).await
    }
    pub async fn update_worker_endpoint(
        self: Arc<Self>,
        pid: u64,
        endpoint_payload: Vec<u8>,
        signature: Vec<u8>,
    ) -> Result<()> {
        let mut endpoint_payload = &endpoint_payload[..];
        let endpoint_payload =
            runtime_types::phala_types::WorkerEndpointPayload::decode(&mut endpoint_payload)?;
        let tx_payload = RuntimeCall::PhalaRegistry(
            khala::runtime_types::phala_pallets::registry::pallet::Call::update_worker_endpoint {
                endpoint_payload,
                signature,
            },
        );
        let desc = format!("Update endpoint of worker for pool #{pid}.");
        self.clone().send_to_queue(pid, tx_payload, desc).await
    }
    pub async fn sync_offchain_message(
        self: Arc<Self>,
        pid: u64,
        signed_message: SignedMessage,
    ) -> Result<()> {
        let tx_payload = RuntimeCall::PhalaMq(
            khala::runtime_types::phala_pallets::mq::pallet::Call::sync_offchain_message {
                signed_message,
            },
        );
        let desc = format!("Sync offchain message to chain for pool #{pid}.");
        self.clone().send_to_queue(pid, tx_payload, desc).await
    }
    pub async fn add_worker(self: Arc<Self>, pid: u64, pubkey: Sr25519Public) -> Result<()> {
        let desc = format!(
            "Add worker 0x{} to pool #{pid}.",
            pubkey.encode_hex::<String>()
        );
        let tx_payload = RuntimeCall::PhalaStakePoolv2(
            khala::runtime_types::phala_pallets::compute::stake_pool_v2::pallet::Call::add_worker {
                pid,
                pubkey,
            },
        );
        self.clone().send_to_queue(pid, tx_payload, desc).await
    }
    pub async fn start_computing(
        self: Arc<Self>,
        pid: u64,
        worker: Sr25519Public,
        stake: String,
    ) -> Result<()> {
        let desc = format!(
            "Start computing for 0x{} with stake of {} in pool #{pid}.",
            worker.encode_hex::<String>(),
            &stake
        );
        let tx_payload = RuntimeCall::PhalaStakePoolv2(
            khala::runtime_types::phala_pallets::compute::stake_pool_v2::pallet::Call::start_computing  {
                pid,
                worker,
                stake: stake.parse::<u128>()?
            },
        );
        self.clone().send_to_queue(pid, tx_payload, desc).await
    }
    pub async fn stop_computing(self: Arc<Self>, pid: u64, worker: Sr25519Public) -> Result<()> {
        let desc = format!(
            "Stop computing for 0x{} in pool #{pid}.",
            worker.encode_hex::<String>()
        );
        let tx_payload = RuntimeCall::PhalaStakePoolv2(
            khala::runtime_types::phala_pallets::compute::stake_pool_v2::pallet::Call::stop_computing  {
                pid,
                worker,
            },
        );
        self.clone().send_to_queue(pid, tx_payload, desc).await
    }
}

#[derive(Clone)]
pub struct PoolOperator {
    pub pid: u64,
    pub pair: Sr25519Pair,
    pub proxied: Option<AccountId32>,
}

#[derive(Clone, Encode, Decode)]
pub struct PoolOperatorForEncode {
    pub pid: u64,
    pub pair: [u8; 96],
    pub proxied: Option<AccountId32>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PoolOperatorForSerialize {
    pub pid: u64,
    pub operator_account_id: String,
    pub proxied_account_id: Option<String>,
}

impl From<&PoolOperator> for PoolOperatorForSerialize {
    fn from(v: &PoolOperator) -> Self {
        let operator_account_id: AccountId32 = v.pair.public().into();
        let operator_account_id = operator_account_id.to_ss58check_with_version(*PHALA_SS58_FORMAT);
        let proxied_account_id = v
            .proxied
            .as_ref()
            .map(|a| a.to_ss58check_with_version(*PHALA_SS58_FORMAT));
        Self {
            pid: v.pid,
            operator_account_id,
            proxied_account_id,
        }
    }
}

impl From<&PoolOperator> for PoolOperatorForEncode {
    fn from(v: &PoolOperator) -> Self {
        let pair = v.pair.as_ref().to_bytes();
        Self {
            pid: v.pid,
            pair,
            proxied: v.proxied.clone(),
        }
    }
}

impl From<&PoolOperatorForEncode> for PoolOperator {
    fn from(v: &PoolOperatorForEncode) -> Self {
        let pair = Sr25519Pair::from(Keypair::from_bytes(v.pair.as_ref()).expect("parse key"));
        Self {
            pid: v.pid,
            pair,
            proxied: v.proxied.clone(),
        }
    }
}

pub trait PoolOperatorAccess {
    fn get_pid_list(&self) -> Result<Vec<u64>>;
    fn set_pid_list(&self, new_list: Vec<u64>) -> Result<Vec<u64>>;
    fn get_all_po(&self) -> Result<Vec<PoolOperator>>;
    fn get_po(&self, pid: u64) -> Result<Option<PoolOperator>>;
    fn set_po(&self, pid: u64, po: PoolOperator) -> Result<PoolOperator>;
}

impl PoolOperatorAccess for DB {
    fn get_pid_list(&self) -> Result<Vec<u64>> {
        let key = PO_LIST.to_string();
        let l = self.get(key)?;
        if l.is_none() {
            return Ok(Vec::new());
        }
        let mut l = &l.unwrap()[..];
        let l: Vec<u64> = Vec::decode(&mut l)?;
        Ok(l)
    }
    fn set_pid_list(&self, new_list: Vec<u64>) -> Result<Vec<u64>> {
        let key = PO_LIST.to_string();
        let b = new_list.encode();
        self.put(key, b)?;
        self.get_pid_list()
    }
    fn get_all_po(&self) -> Result<Vec<PoolOperator>> {
        let curr_pid_list = self.get_pid_list()?;
        let mut ret = Vec::new();
        for id in curr_pid_list {
            let i = self
                .get_po(id)?
                .ok_or(anyhow!(format!("po record #{id} not found!")))?;
            ret.push(i);
        }
        Ok(ret)
    }
    fn get_po(&self, pid: u64) -> Result<Option<PoolOperator>> {
        let key = format!("{PO_BY_PID}:{pid}");
        let b = self.get(key)?;
        if b.is_none() {
            return Ok(None);
        }
        let mut b = &b.unwrap()[..];
        let po = PoolOperatorForEncode::decode(&mut b)?;
        Ok(Some((&po).into()))
    }
    fn set_po(&self, pid: u64, po: PoolOperator) -> Result<PoolOperator> {
        let mut pl = self.get_pid_list()?;
        pl.retain(|&i| i != pid);
        pl.push(pid);
        let key = format!("{PO_BY_PID}:{pid}");
        let b = PoolOperatorForEncode::from(&po);
        let b = b.encode();
        self.put(key, b)?;
        let r = self.get_po(pid)?;
        let _ = self.set_pid_list(pl)?;
        Ok(r.unwrap())
    }
}