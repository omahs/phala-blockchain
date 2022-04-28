use crate::GRANDPA_ENGINE_ID;
use anyhow::{anyhow, Result};
use codec::{Decode, Encode};
use phactory_api::blocks::AuthoritySetChange;
use phaxt::{
    subxt::{self, rpc::NumberOrHex},
    BlockNumber, Header, RelaychainApi,
};
use std::{
    io::{Read, Write},
    mem::replace,
};

pub use phactory_api::blocks::GenesisBlockInfo;

#[derive(Decode, Encode)]
pub struct BlockInfo {
    pub header: Header,
    pub justification: Option<Vec<u8>>,
    pub para_header: Option<ParaHeader>,
    pub authority_set_change: Option<AuthoritySetChange>,
}

#[derive(Decode, Encode)]
pub struct ParaHeader {
    /// Finalized parachain header number
    pub fin_header_num: BlockNumber,
    pub proof: Vec<Vec<u8>>,
}

pub trait DB {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
}

/// Import header logs into database.
pub async fn import_headers(mut input: impl Read, to_db: &mut impl DB) -> Result<u32> {
    let mut count = 0_u32;
    let mut buffer = vec![0u8; 1024 * 100];
    loop {
        let mut length_buf = [0u8; 4];
        if input.read(&mut length_buf)? != 4 {
            break;
        }
        let length = u32::from_be_bytes(length_buf) as usize;
        if length > buffer.len() {
            buffer.resize(length, 0);
        }
        let buf = &mut buffer[..length];
        input.read_exact(buf)?;
        let header: Header = Decode::decode(&mut &buf[..])?;
        to_db.put(&header.number.to_be_bytes(), buf)?;
        count += 1;
    }
    Ok(count)
}

/// Dump headers from the chain to a log file.
pub async fn grap_headers_to_file(
    api: &RelaychainApi,
    start_at: BlockNumber,
    count: BlockNumber,
    justification_interval: BlockNumber,
    mut output: impl Write,
) -> Result<BlockNumber> {
    grab_headers(api, start_at, count, justification_interval, |info| {
        if info.header.number % 1024 == 0 {
            log::info!("Grabbed to {}", info.header.number);
        }
        let encoded = info.encode();
        let length = encoded.len() as u32;
        output.write_all(length.to_be_bytes().as_ref())?;
        output.write_all(&encoded)?;
        Ok(())
    })
    .await
}

async fn grab_headers(
    api: &RelaychainApi,
    start_at: BlockNumber,
    count: BlockNumber,
    justification_interval: u32,
    mut f: impl FnMut(BlockInfo) -> Result<()>,
) -> Result<BlockNumber> {
    if start_at == 0 {
        anyhow::bail!("start block must be > 0");
    }
    if count == 0 {
        return Ok(0);
    }
    let (mut last_header, mut last_header_hash) =
        crate::get_header_at(&api.client, Some(start_at)).await?;
    let mut last_justifications = None;
    let mut last_set = api
        .storage()
        .grandpa()
        .current_set_id(Some(last_header_hash))
        .await?;
    let mut skip_justitication = justification_interval;
    let mut grabbed = 0;

    for block_number in start_at + 1.. {
        let header;
        let justifications;
        let hash;
        if skip_justitication == 0 {
            let (block, header_hash) =
                match crate::get_block_at(&api.client, Some(block_number)).await {
                    Ok(x) => x,
                    Err(e) => {
                        if e.to_string().contains("not found") {
                            break;
                        }
                        return Err(e);
                    }
                };
            header = block.block.header;
            justifications = block.justifications;
            hash = header_hash;
        } else {
            let (hdr, hdr_hash) = match crate::get_header_at(&api.client, Some(block_number)).await
            {
                Ok(x) => x,
                Err(e) => {
                    if e.to_string().contains("not found") {
                        break;
                    }
                    return Err(e);
                }
            };
            header = hdr;
            hash = hdr_hash;
            justifications = None;
        };
        if header.parent_hash != last_header_hash {
            anyhow::bail!(
                "parent hash mismatch, block={}, parent={}, last={}",
                header.number,
                header.parent_hash,
                last_header_hash
            );
        }
        let set_id = api.storage().grandpa().current_set_id(Some(hash)).await?;
        let authority_set_change = if last_set != set_id {
            if last_justifications.is_none() {
                last_justifications = api
                    .client
                    .rpc()
                    .block(Some(last_header_hash.clone()))
                    .await?
                    .ok_or(anyhow!("No justification for block changing set_id"))?
                    .justifications;
            }
            Some(crate::get_authority_with_proof_at(&api, last_header_hash).await?)
        } else {
            None
        };

        skip_justitication = skip_justitication.saturating_sub(1);

        let last_header = replace(&mut last_header, header);
        let last_justifications = replace(&mut last_justifications, justifications);
        last_set = set_id;
        last_header_hash = hash;

        let justification = last_justifications
            .map(|v| v.into_justification(GRANDPA_ENGINE_ID))
            .flatten();

        if justification.is_some() {
            skip_justitication = justification_interval;
        }

        f(BlockInfo {
            header: last_header,
            justification,
            authority_set_change,
        })?;
        grabbed += 1;
        if count == grabbed {
            break;
        }
    }
    Ok(grabbed)
}

pub async fn fetch_genesis_info(
    api: &RelaychainApi,
    genesis_block_number: BlockNumber,
) -> Result<GenesisBlockInfo> {
    let genesis_block = crate::get_block_at(&api.client, Some(genesis_block_number))
        .await?
        .0
        .block;
    let hash = api
        .client
        .rpc()
        .block_hash(Some(subxt::BlockNumber::from(NumberOrHex::Number(
            genesis_block_number as _,
        ))))
        .await?
        .expect("No genesis block?");
    let set_proof = crate::get_authority_with_proof_at(api, hash).await?;
    Ok(GenesisBlockInfo {
        block_header: genesis_block.header,
        authority_set: set_proof.authority_set,
        proof: set_proof.authority_proof,
    })
}

#[derive(Clone)]
pub(crate) struct Client {
    base_uri: String,
}

impl Client {
    pub fn new(uri: &str) -> Self {
        Self {
            base_uri: uri.to_string(),
        }
    }

    async fn request<T: Decode>(&self, url: &str) -> Result<T> {
        let body = reqwest::get(url).await?.bytes().await?;
        Ok(T::decode(&mut &body[..])?)
    }

    pub async fn get_header(&self, block_number: BlockNumber) -> Result<BlockInfo> {
        let url = format!("{}/header/{}", self.base_uri, block_number);
        self.request(&url).await
    }

    pub async fn get_genesis(&self, block_number: BlockNumber) -> Result<GenesisBlockInfo> {
        let url = format!("{}/genesis/{}", self.base_uri, block_number);
        self.request(&url).await
    }
}
