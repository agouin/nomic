use super::SignatorySet;
use bitcoin::blockdata::transaction::EcdsaSighashType;
use bitcoin::secp256k1::{
    self,
    constants::{COMPACT_SIGNATURE_SIZE, MESSAGE_SIZE, PUBLIC_KEY_SIZE},
    ecdsa, PublicKey, Secp256k1,
};
use orga::call::Call;
use orga::client::Client;
use orga::collections::{Map, Next};
use orga::encoding::{Decode, Encode, Error as EdError, Result as EdResult, Terminated};
use orga::query::Query;
use orga::state::State;
use orga::{Error, Result};

pub type Message = [u8; MESSAGE_SIZE];
pub type Signature = [u8; COMPACT_SIGNATURE_SIZE];

#[derive(
    Encode, Decode, State, Query, Call, Client, Clone, Debug, Copy, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct Pubkey([u8; PUBLIC_KEY_SIZE]);

impl Next for Pubkey {
    fn next(&self) -> Option<Self> {
        let mut output = self.clone();
        for (i, value) in self.0.iter().enumerate().rev() {
            match value.next() {
                Some(new_value) => {
                    output.0[i] = new_value;
                    return Some(output);
                }
                None => {
                    output.0[i] = 0;
                }
            }
        }
        None
    }
}

impl Default for Pubkey {
    fn default() -> Self {
        Pubkey([0; PUBLIC_KEY_SIZE])
    }
}

impl Pubkey {
    pub fn new(pubkey: [u8; PUBLIC_KEY_SIZE]) -> Self {
        Pubkey(pubkey)
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl From<PublicKey> for Pubkey {
    fn from(pubkey: PublicKey) -> Self {
        Pubkey(pubkey.serialize())
    }
}

// TODO: update for taproot-based design (musig rounds, fallback path)

#[derive(State, Call, Client, Query)]
pub struct ThresholdSig {
    threshold: u64,
    signed: u64,
    message: Message,
    len: u16,
    sigs: Map<Pubkey, Share>,
}

impl ThresholdSig {
    pub fn len(&self) -> u16 {
        self.len
    }

    pub fn set_message(&mut self, message: Message) {
        self.message = message;
    }

    pub fn message(&self) -> Message {
        self.message
    }

    pub fn from_sigset(&mut self, signatories: &SignatorySet) -> Result<()> {
        let mut total_vp = 0;

        for signatory in signatories.iter() {
            self.sigs.insert(
                signatory.pubkey,
                Share {
                    power: signatory.voting_power,
                    sig: None,
                }
                .into(),
            )?;

            self.len += 1;
            total_vp += signatory.voting_power;
        }

        // TODO: get threshold ratio from somewhere else
        self.threshold = ((total_vp as u128) * 2 / 3) as u64;

        Ok(())
    }

    pub fn from_shares(&mut self, shares: Vec<(Pubkey, Share)>) -> Result<()> {
        let mut total_vp = 0;
        let mut len = 0;

        for (pubkey, share) in shares.into_iter() {
            assert!(share.sig.is_none());
            total_vp += share.power;
            len += 1;
            self.sigs.insert(pubkey, share.into())?;
        }

        // TODO: get threshold ratio from somewhere else
        self.threshold = ((total_vp as u128) * 2 / 3) as u64;
        self.len = len;

        Ok(())
    }

    #[query]
    pub fn done(&self) -> bool {
        self.signed >= self.threshold
    }

    #[query]
    pub fn sigs(&self) -> Result<Vec<(Pubkey, Signature)>> {
        self.sigs
            .iter()?
            .filter_map(|entry| {
                let (pubkey, share) = match entry {
                    Err(e) => return Some(Err(e)),
                    Ok(entry) => entry,
                };
                share
                    .sig
                    .as_ref()
                    .map(|sig| Ok((pubkey.clone(), sig.clone())))
            })
            .collect()
    }

    // TODO: should be iterator?
    pub fn shares(&self) -> Result<Vec<(Pubkey, Share)>> {
        self.sigs
            .iter()?
            .map(|entry| entry.map(|(pubkey, share)| (pubkey.clone(), share.clone())))
            .collect()
    }

    #[query]
    pub fn contains_key(&self, pubkey: Pubkey) -> Result<bool> {
        self.sigs.contains_key(pubkey)
    }

    #[query]
    pub fn needs_sig(&self, pubkey: Pubkey) -> Result<bool> {
        Ok(self
            .sigs
            .get(pubkey)?
            .map(|share| share.sig.is_none())
            .unwrap_or(false))
    }

    // TODO: exempt from fee
    pub fn sign(&mut self, pubkey: Pubkey, sig: Signature) -> Result<()> {
        if self.done() {
            return Err(Error::App("Threshold signature is done".into()));
        }

        let share = self
            .sigs
            .get(pubkey)?
            .ok_or_else(|| Error::App("Pubkey is not part of threshold signature".into()))?;

        if share.sig.is_some() {
            return Err(Error::App("Pubkey already signed".into()));
        }

        self.verify(pubkey, sig)?;

        let mut share = self
            .sigs
            .get_mut(pubkey)?
            .ok_or_else(|| Error::App("Pubkey is not part of threshold signature".into()))?;

        share.sig = Some(sig);
        self.signed += share.power;

        Ok(())
    }

    pub fn verify(&self, pubkey: Pubkey, sig: Signature) -> Result<()> {
        // TODO: re-use secp context
        let secp = Secp256k1::verification_only();
        let pubkey = PublicKey::from_slice(&pubkey.0)?;
        let msg = secp256k1::Message::from_slice(self.message.as_slice())?;
        let sig = ecdsa::Signature::from_compact(sig.as_slice())?;

        #[cfg(not(fuzzing))]
        secp.verify_ecdsa(&msg, &sig, &pubkey)?;

        Ok(())
    }

    // TODO: this shouldn't know so much about bitcoin-specific structure,
    // decouple by exposing a power-ordered iterator of Option<Signature>
    pub fn to_witness(&self) -> Result<Vec<Vec<u8>>> {
        if !self.done() {
            return Ok(vec![]);
        }

        let mut entries: Vec<_> = self.sigs.iter()?.collect::<Result<_>>()?;
        entries.sort_by(|a, b| (a.1.power, &a.0).cmp(&(b.1.power, &b.0)));

        entries
            .into_iter()
            .map(|(_, share)| {
                share.sig.map_or(Ok(vec![]), |sig| {
                    let sig = ecdsa::Signature::from_compact(sig.as_slice())?;
                    let mut v = sig.serialize_der().to_vec();
                    v.push(EcdsaSighashType::All.to_u32() as u8);
                    Ok(v)
                })
            })
            .collect()
    }
}

use std::fmt::Debug;
impl Debug for ThresholdSig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThresholdSig")
            .field("threshold", &self.threshold)
            .field("signed", &self.signed)
            .field("message", &self.message)
            .field("len", &self.len)
            .field("sigs", &"TODO")
            .finish()
    }
}

#[derive(State, Call, Client, Query, Clone)]
pub struct Share {
    power: u64,
    sig: Option<Signature>,
}

// TODO: move this into ed
use derive_more::{Deref, DerefMut, Into};
use std::convert::{TryFrom, TryInto};

#[derive(Deref, DerefMut, Encode, Into, Default)]
pub struct LengthVec<P, T>
where
    P: Encode + Terminated,
    T: Encode + Terminated,
{
    len: P,

    #[deref]
    #[deref_mut]
    #[into]
    values: Vec<T>,
}

impl<P, T> LengthVec<P, T>
where
    P: Encode + Terminated,
    T: Encode + Terminated,
{
    pub fn new(len: P, values: Vec<T>) -> Self {
        LengthVec { len, values }
    }
}

impl<P, T> State for LengthVec<P, T>
where
    P: Encode + Decode + Terminated + TryInto<usize> + Clone,
    T: Encode + Decode + Terminated,
{
    type Encoding = Self;

    fn create(_: orga::store::Store, data: Self::Encoding) -> Result<Self> {
        Ok(data)
    }

    fn flush(self) -> Result<Self::Encoding> {
        Ok(self)
    }
}

impl<P, T> From<Vec<T>> for LengthVec<P, T>
where
    P: Encode + Terminated + TryFrom<usize>,
    T: Encode + Terminated,
    <P as TryFrom<usize>>::Error: std::fmt::Debug,
{
    fn from(values: Vec<T>) -> Self {
        LengthVec::new(P::try_from(values.len()).unwrap(), values)
    }
}

impl<P, T> Terminated for LengthVec<P, T>
where
    P: Encode + Terminated,
    T: Encode + Terminated,
{
}

impl<P, T> Decode for LengthVec<P, T>
where
    P: Encode + Decode + Terminated + TryInto<usize> + Clone,
    T: Encode + Decode + Terminated,
{
    fn decode<R: std::io::Read>(mut input: R) -> EdResult<Self> {
        let len = P::decode(&mut input)?;
        let len_usize = len
            .clone()
            .try_into()
            .map_err(|_| EdError::UnexpectedByte(80))?;

        let mut values = Vec::with_capacity(len_usize);
        for _ in 0..len_usize {
            let value = T::decode(&mut input)?;
            values.push(value);
        }

        Ok(LengthVec { len, values })
    }
}
