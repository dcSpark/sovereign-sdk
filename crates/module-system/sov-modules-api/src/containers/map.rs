use std::marker::PhantomData;
use std::str::FromStr;

#[cfg(feature = "native")]
use anyhow::ensure;
use sov_state::codec::BorshCodec;
use sov_state::namespaces::{Accessory, CompileTimeNamespace, Kernel, User};
use sov_state::{EncodeLike, Prefix, SlotKey, SlotValue, StateCodec, StateItemCodec};
#[cfg(feature = "native")]
use sov_state::{StateItemDecoder, Storage};
use thiserror::Error;
#[cfg(feature = "arbitrary")]
use unwrap_infallible::UnwrapInfallible;

use super::{Borrowed, BorrowedMut};
use crate::state::StateReader;
#[cfg(feature = "native")]
use crate::ProvenStateAccessor;
#[cfg(feature = "native")]
use crate::StateCheckpoint;
#[cfg(feature = "arbitrary")]
use crate::InfallibleStateReaderAndWriter;
use crate::{StateReaderAndWriter, StateWriter};

/// A container that maps keys to values.
///
/// # Type parameters
/// [`StateMap`] is generic over:
/// - a key type `K`;
/// - a value type `V`;
/// - a [`sov_state::StateItemCodec`] `Codec`.
#[derive(
    Debug,
    Clone,
    PartialEq,
    borsh::BorshDeserialize,
    borsh::BorshSerialize,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct NamespacedStateMap<N, K, V, Codec = BorshCodec>
where
    N: CompileTimeNamespace,
    Codec: StateCodec,
    Codec::KeyCodec: StateItemCodec<K>,
    Codec::ValueCodec: StateItemCodec<V>,
    K: FromStr + std::fmt::Display,
{
    _phantom: PhantomData<(N, K, V)>,
    pub(crate) codec: Codec,
    pub(crate) prefix: Prefix,
}

/// Error type for the get method of state maps.
#[derive(Debug, Error)]
pub enum StateMapError<N> {
    /// Value not found.
    #[error("Value not found for prefix: {0} and storage key: {1} in namespace {}", std::any::type_name::<N>())]
    MissingValue(Prefix, SlotKey, PhantomData<N>),
}

type ValueOrError<V, N> = Result<V, StateMapError<N>>;

/// A container that maps keys to values
///
/// # Type parameters
/// [`StateMap`] is generic over:
/// - a key type `K`;
/// - a value type `V`;
/// - a  [`Codec`](`sov_state::StateItemCodec`).
pub type StateMap<K, V, Codec = BorshCodec> = NamespacedStateMap<User, K, V, Codec>;

/// A container that maps keys to values stored as "accessory" state, outside of
/// the JMT.
///
/// # Type parameters
/// [`AccessoryStateMap`] is generic over:
/// - a key type `K`;
/// - a value type `V`;
/// - a  [`Codec`](`sov_state::StateItemCodec`).
pub type AccessoryStateMap<K, V, Codec = BorshCodec> = NamespacedStateMap<Accessory, K, V, Codec>;

/// A container that maps keys to values which are only accessible the kernel.
///
/// # Type parameters
/// [`KernelStateMap`] is generic over:
/// - a key type `K`;
/// - a value type `V`;
/// - a  [`Codec`](`sov_state::StateItemCodec`).
pub type KernelStateMap<K, V, Codec = BorshCodec> = NamespacedStateMap<Kernel, K, V, Codec>;

impl<N, K, V, Codec> NamespacedStateMap<N, K, V, Codec>
where
    N: CompileTimeNamespace,
    Codec: StateCodec,
    Codec::KeyCodec: StateItemCodec<K>,
    Codec::ValueCodec: StateItemCodec<V>,
    K: FromStr + std::fmt::Display,
{
    //e Creates a new [`StateMap`] with the given prefix and [`sov_modules_core::StateItemCodec`].
    pub fn with_codec(prefix: Prefix, codec: Codec) -> Self {
        Self {
            _phantom: PhantomData,
            codec,
            prefix,
        }
    }

    /// Returns the prefix used when this [`StateMap`] was created.
    pub fn prefix(&self) -> &Prefix {
        &self.prefix
    }

    /// Returns the codec used when this [`StateMap`] was created.
    pub fn codec(&self) -> &Codec {
        &self.codec
    }
}

impl<N, K, V, Codec> NamespacedStateMap<N, K, V, Codec>
where
    N: CompileTimeNamespace,
    Codec: StateCodec,
    Codec::KeyCodec: StateItemCodec<K>,
    Codec::ValueCodec: StateItemCodec<V>,
    K: FromStr + std::fmt::Display,
{
    pub fn slot_key<Kq>(&self, key: &Kq) -> SlotKey
    where
        Kq: ?Sized,
        Codec::KeyCodec: EncodeLike<Kq, K>,
    {
        SlotKey::new(self.prefix(), key, self.codec().key_codec())
    }

    pub(super) fn slot_value<Vq>(&self, value: &Vq) -> SlotValue
    where
        Vq: ?Sized,
        Codec::ValueCodec: EncodeLike<Vq, V>,
    {
        SlotValue::new(value, self.codec().value_codec())
    }

    /// Inserts a key-value pair into the map.
    ///
    /// The key may be any borrowed form of the
    /// map’s key type.
    pub fn set<Kq, Vq, Writer>(
        &mut self,
        key: &Kq,
        value: &Vq,
        state: &mut Writer,
    ) -> Result<(), Writer::Error>
    where
        Codec::KeyCodec: EncodeLike<Kq, K>,
        Codec::ValueCodec: EncodeLike<Vq, V>,
        Kq: ?Sized,
        Vq: ?Sized,
        Writer: StateWriter<N>,
    {
        let key = self.slot_key(key);
        tracing::trace!(%key, "Setting map value");
        state.set(&key, self.slot_value(value))
    }

    /// Calls [`StateMap::set`] iff the key is not already present in the map.
    pub fn set_if_absent<Kq, Vq, Writer>(
        &mut self,
        key: &Kq,
        value: &Vq,
        state: &mut Writer,
    ) -> Result<(), <Writer as StateWriter<N>>::Error>
    where
        Codec::KeyCodec: EncodeLike<Kq, K>,
        Codec::ValueCodec: EncodeLike<Vq, V>,
        Kq: ?Sized,
        Vq: ?Sized,
        Writer: StateReaderAndWriter<N>,
    {
        if state
            .get_decoded(&self.slot_key(key), self.codec())?
            .is_none()
        {
            self.set(key, value, state)?;
        }

        Ok(())
    }

    /// Returns the value corresponding to the key, or [`None`] if the map
    /// doesn't contain the key.
    ///
    /// # Examples
    ///
    /// The key may be any item that implements [`EncodeLike`] the map's key type
    /// using your chosen codec.
    ///
    /// ```
    /// use sov_modules_api::{Spec, Context, HexString, StateMap, WorkingSet, StateAccessorError};
    ///
    /// fn foo<S: Spec>(map: StateMap<HexString, u64>, key: &[u8], state: &mut WorkingSet<S>) -> Result<Option<u64>, StateAccessorError<S::Gas>>
    /// {
    ///     // We perform the `get` with a slice, and not a owned `HexString`. This works because a `HexString` is just
    ///     // a wrapper around `Vec<u8>` that implements `Display` and `FromStr` - so we can encode any byte slice
    ///     // like a HexString.
    ///     map.get(key, state)
    /// }
    /// ```
    ///
    /// If the map's key type does not implement [`EncodeLike`] for your desired
    /// target type, you'll have to convert the key to something else. An
    /// example of this would be "slicing" an array to use in [`sov_modules_api::HexString`]-keyed
    /// maps:
    ///
    /// ```
    /// use sov_modules_api::{Spec, Context, HexString, StateMap, WorkingSet, StateAccessorError};
    ///
    /// fn foo<S: Spec>(map: StateMap<HexString, u64>, key: [u8; 32], state: &mut WorkingSet<S>) -> Result<Option<u64>, StateAccessorError<S::Gas>>
    /// {
    ///     map.get(key.as_ref(), state)
    /// }
    /// ```
    pub fn get<Kq, Reader>(&self, key: &Kq, state: &mut Reader) -> Result<Option<V>, Reader::Error>
    where
        Codec::KeyCodec: EncodeLike<Kq, K>,
        Kq: ?Sized,
        Reader: StateReader<N>,
    {
        let key = self.slot_key(key);
        tracing::trace!(%key, "Getting map value");
        state.get_decoded(&key, self.codec())
    }

    /// Returns a borrowed value from the map, preventing mutable access to the map until this reference is dropped.
    pub fn borrow<Kq, Reader>(
        &self,
        key: &Kq,
        state: &mut Reader,
    ) -> Result<Borrowed<Option<V>, Self>, Reader::Error>
    where
        Codec::KeyCodec: EncodeLike<Kq, K>,
        Kq: ?Sized,
        Reader: StateReader<N>,
    {
        let key = self.slot_key(key);
        tracing::trace!(%key, "Borrowing map value");
        let val = state.get_decoded(&key, self.codec())?;
        Ok(Borrowed::new(val, self))
    }

    /// Returns a mutably borrowed value from the map, preventing access to the map until this reference is dropped.
    pub fn borrow_mut<Kq, Reader>(
        &mut self,
        key: &Kq,
        state: &mut Reader,
    ) -> Result<BorrowedMut<Option<V>, Self>, Reader::Error>
    where
        Codec::KeyCodec: EncodeLike<Kq, K>,
        Kq: ?Sized,
        Reader: StateReader<N>,
    {
        let key = self.slot_key(key);
        tracing::trace!(%key, "Borrowing map value");
        let val = state.get_decoded(&key, self.codec())?;
        Ok(BorrowedMut::new(key, val, self))
    }

    /// Returns the value corresponding to the key or [`StateMapError`] if key is absent from
    /// the map.
    pub fn get_or_err<Kq, Reader>(
        &self,
        key: &Kq,
        state: &mut Reader,
    ) -> Result<ValueOrError<V, N>, Reader::Error>
    where
        Codec::KeyCodec: EncodeLike<Kq, K>,
        Kq: ?Sized,
        Reader: StateReader<N>,
    {
        Ok(self.get(key, state)?.ok_or_else(|| {
            StateMapError::MissingValue(
                self.prefix().clone(),
                SlotKey::new(self.prefix(), key, self.codec().key_codec()),
                PhantomData,
            )
        }))
    }

    /// Removes a key from the map, returning the corresponding value (or
    /// [`None`] if the key is absent).
    pub fn remove<Kq, ReaderAndWriter>(
        &self,
        key: &Kq,
        state: &mut ReaderAndWriter,
    ) -> Result<Option<V>, <ReaderAndWriter as StateWriter<N>>::Error>
    where
        Codec::KeyCodec: EncodeLike<Kq, K>,
        Kq: ?Sized,
        ReaderAndWriter: StateReaderAndWriter<N>,
    {
        let key = self.slot_key(key);
        tracing::trace!(%key, "Removing map value");
        state.remove_decoded(&key, self.codec())
    }

    /// Removes a key from the map, returning the corresponding value (or
    /// [`StateMapError`] if the key is absent).
    ///
    /// Use [`NamespacedStateMap::remove`] if you want an [`Option`] instead of a [`Result`].
    pub fn remove_or_err<Kq, ReaderAndWriter>(
        &mut self,
        key: &Kq,
        state: &mut ReaderAndWriter,
    ) -> Result<ValueOrError<V, N>, <ReaderAndWriter as StateWriter<N>>::Error>
    where
        Codec::KeyCodec: EncodeLike<Kq, K>,
        Kq: ?Sized,
        ReaderAndWriter: StateReaderAndWriter<N>,
    {
        Ok(self.remove(key, state)?.ok_or_else(|| {
            StateMapError::MissingValue(
                self.prefix().clone(),
                SlotKey::new(self.prefix(), key, self.codec().key_codec()),
                PhantomData,
            )
        }))
    }

    /// Deletes a key-value pair from the map.
    ///
    /// This is equivalent to [`NamespacedStateMap::remove`], but doesn't deserialize and
    /// return the value before deletion, or error on a missing value.
    pub fn delete<Kq, Writer>(&mut self, key: &Kq, state: &mut Writer) -> Result<(), Writer::Error>
    where
        Codec: StateCodec,
        Codec::KeyCodec: EncodeLike<Kq, K>,
        Kq: ?Sized,
        Writer: StateWriter<N>,
    {
        let key = self.slot_key(key);
        tracing::trace!(%key, "Deleting map value");
        state.delete(&key)
    }

    /// Returns the raw prefix bytes for this map (without any key prefix).
    pub fn raw_prefix_bytes(&self) -> &[u8] {
        self.prefix.as_ref()
    }
}

#[cfg(feature = "native")]
impl<N: sov_state::namespaces::ProvableCompileTimeNamespace, K, V, Codec>
    NamespacedStateMap<N, K, V, Codec>
where
    Codec: StateCodec,
    Codec::ValueCodec: StateItemCodec<V>,
    Codec::KeyCodec: StateItemCodec<K>,
    K: FromStr + std::fmt::Display,
{
    pub fn get_with_proof<Kq, W>(
        &self,
        key: &Kq,
        state: &mut W,
    ) -> Option<sov_state::StorageProof<W::Proof>>
    where
        Kq: ?Sized,
        Codec::KeyCodec: EncodeLike<Kq, K>,
        W: ProvenStateAccessor<N>,
    {
        state.get_with_proof(self.slot_key(key))
    }

    pub fn verify_proof<S: crate::Spec>(
        &self,
        state_root: <S::Storage as Storage>::Root,
        proof: sov_state::StorageProof<<<S as crate::Spec>::Storage as Storage>::Proof>,
    ) -> anyhow::Result<(K, Option<V>)>
where {
        ensure!(
            proof.namespace == N::PROVABLE_NAMESPACE,
            "The provided proof comes from a different namespace. Expected {:?} but found {:?}",
            N::PROVABLE_NAMESPACE,
            proof.namespace
        );

        let (complete_key, value) = <<S as crate::Spec>::Storage>::open_proof(state_root, proof)?;
        let complete_key_bytes = complete_key.key();
        let item_key = complete_key_bytes
            .strip_prefix(self.prefix().as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!("The key in the proof did not match the expected key. Expected key with prefix: {:?}, found key: {:?}", self.prefix(), complete_key.key())
            })?;

        let item_key = self
            .codec()
            .key_codec()
            .try_decode(item_key)
            .map_err(|e| anyhow::anyhow!("Failed to decode key from proof: {:?}", e))?;

        let value = value
            .map(|v| {
                self.codec()
                    .value_codec()
                    .try_decode(v.value())
                    .map_err(|e| anyhow::anyhow!("Failed to decode value from proof: {:?}", e))
            })
            .transpose()?;
        Ok((item_key, value))
    }
}

/// Prefix iteration support for StateMap (User namespace only).
/// This enables enumeration of keys matching a given prefix, which is essential
/// for parallel-safe storage patterns where each transaction writes to unique keys
/// and we need to enumerate them at flush time.
#[cfg(feature = "native")]
impl<K, V, Codec> NamespacedStateMap<User, K, V, Codec>
where
    Codec: StateCodec,
    Codec::ValueCodec: StateItemCodec<V> + StateItemDecoder<V>,
    Codec::KeyCodec: StateItemCodec<K> + StateItemDecoder<K>,
    K: FromStr + std::fmt::Display,
    <Codec::KeyCodec as StateItemDecoder<K>>::Error: std::error::Error + Send + Sync + 'static,
    <Codec::ValueCodec as StateItemDecoder<V>>::Error: std::error::Error + Send + Sync + 'static,
{
    /// Iterates over all key-value pairs in this map whose key starts with `key_prefix`.
    ///
    /// This is used for parallel-safe enumeration: each transaction writes to unique keys
    /// (e.g., `(height, commitment)`), and at flush time we enumerate all keys for the
    /// current height using this method.
    ///
    /// # Arguments
    /// * `key_prefix` - A prefix struct that, when Borsh-serialized, produces the prefix
    ///   bytes to match against. For example, `PendingCommitmentPrefix { height: 42 }` will
    ///   match all `PendingCommitmentKey { height: 42, commitment: ... }` keys.
    /// * `state` - The StateCheckpoint to iterate over.
    ///
    /// # Returns
    /// A vector of (key, value) pairs matching the prefix.
    ///
    /// # Example
    /// ```ignore
    /// // Define prefix type (must serialize to prefix of full key)
    /// #[derive(BorshSerialize)]
    /// struct PendingCommitmentPrefix { height: u64 }
    ///
    /// // Enumerate all commitments for height 42
    /// let prefix = PendingCommitmentPrefix { height: 42 };
    /// let entries = map.iter_prefix(&prefix, &mut checkpoint)?;
    /// for (key, value) in entries {
    ///     // Process each matching entry
    /// }
    /// ```
    pub fn iter_prefix<P, S>(
        &self,
        key_prefix: &P,
        state: &mut StateCheckpoint<S>,
    ) -> anyhow::Result<Vec<(K, V)>>
    where
        P: borsh::BorshSerialize,
        S: crate::Spec,
    {
        // Build the full prefix: map prefix + serialized key prefix
        let mut full_prefix = self.prefix.as_ref().to_vec();
        let key_prefix_bytes = borsh::to_vec(key_prefix)?;
        full_prefix.extend_from_slice(&key_prefix_bytes);

        let mut results = Vec::new();
        let map_prefix_len = self.prefix.as_ref().len();

        // Iterate over all writes matching the prefix
        for (slot_key, maybe_value) in state.iter_user_prefix_writes(&full_prefix) {
            // Only process if there's a value (not deleted)
            if let Some(value) = maybe_value {
                // Strip the map prefix to get the raw key bytes
                let key_bytes = slot_key.key_ref();
                if key_bytes.len() > map_prefix_len {
                    let item_key_bytes = &key_bytes[map_prefix_len..];

                    // Decode key and value
                    let key = self.codec.key_codec().try_decode(item_key_bytes)?;
                    let val = self.codec.value_codec().try_decode(value.value())?;
                    results.push((key, val));
                }
            }
        }

        Ok(results)
    }

    /// Deletes all entries matching the given prefix.
    ///
    /// This is typically called after `iter_prefix` to clean up processed entries.
    pub fn delete_prefix<P, S>(
        &mut self,
        key_prefix: &P,
        state: &mut StateCheckpoint<S>,
    ) -> anyhow::Result<()>
    where
        P: borsh::BorshSerialize,
        S: crate::Spec,
    {
        // Build the full prefix: map prefix + serialized key prefix
        let mut full_prefix = self.prefix.as_ref().to_vec();
        let key_prefix_bytes = borsh::to_vec(key_prefix)?;
        full_prefix.extend_from_slice(&key_prefix_bytes);

        let map_prefix_len = self.prefix.as_ref().len();

        // Collect keys to delete (we need to collect first to avoid borrow issues)
        let keys_to_delete: Vec<K> = state
            .iter_user_prefix_writes(&full_prefix)
            .filter_map(|(slot_key, maybe_value)| {
                if maybe_value.is_some() {
                    let key_bytes = slot_key.key_ref();
                    if key_bytes.len() > map_prefix_len {
                        let item_key_bytes = &key_bytes[map_prefix_len..];
                        self.codec.key_codec().try_decode(item_key_bytes).ok()
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        // Delete each key
        for key in keys_to_delete {
            self.delete(&key, state)?;
        }

        Ok(())
    }
}

#[cfg(feature = "arbitrary")]
impl<'a, N, K, V, Codec> NamespacedStateMap<N, K, V, Codec>
where
    K: arbitrary::Arbitrary<'a> + FromStr + std::fmt::Display,
    V: arbitrary::Arbitrary<'a>,
    Codec: sov_state::StateCodec,
    Codec::KeyCodec: sov_state::StateItemCodec<K>,
    Codec::ValueCodec: sov_state::StateItemCodec<V>,
    N: CompileTimeNamespace,
{
    /// Returns an arbitrary [`StateMap`] instance.
    ///
    /// See the [`arbitrary`] crate for more information.
    pub fn arbitrary_state_map<S>(
        u: &mut arbitrary::Unstructured<'a>,
        working_set: &mut crate::StateCheckpoint<S>,
    ) -> arbitrary::Result<Self>
    where
        S: crate::Spec,
        StateCheckpoint<S>: InfallibleStateReaderAndWriter<N>,
    {
        use arbitrary::Arbitrary;

        let prefix = Prefix::arbitrary(u)?;
        let len = u.arbitrary_len::<(K, V)>()?;
        let codec = Codec::default();
        let map = Self::with_codec(prefix, codec);

        (0..len).try_fold(map, |mut map, _| {
            let key = K::arbitrary(u)?;
            let value = V::arbitrary(u)?;

            map.set(&key, &value, working_set).unwrap_infallible();

            Ok(map)
        })
    }
}
