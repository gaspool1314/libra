// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    access_path::AccessPath,
    account_address::AccountAddress,
    account_config::AccountResource,
    account_state_blob::AccountStateBlob,
    byte_array::ByteArray,
    contract_event::ContractEvent,
    event::{EventHandle, EventKey},
    get_with_proof::{ResponseItem, UpdateToLatestLedgerResponse},
    ledger_info::{LedgerInfo, LedgerInfoWithSignatures},
    proof::AccumulatorProof,
    transaction::{
        Module, Program, RawTransaction, Script, SignatureCheckedTransaction, SignedTransaction,
        TransactionArgument, TransactionInfo, TransactionListWithProof, TransactionPayload,
        TransactionStatus, TransactionToCommit, Version,
    },
    validator_change::ValidatorChangeEventWithProof,
    vm_error::{StatusCode, VMStatus},
    write_set::{WriteOp, WriteSet, WriteSetMut},
};
use crypto::{
    ed25519::{compat::keypair_strategy, *},
    hash::CryptoHash,
    traits::*,
    HashValue,
};
use proptest::{
    collection::{vec, SizeRange},
    option,
    prelude::*,
};
use proptest_derive::Arbitrary;
use proptest_helpers::Index;
use std::time::Duration;

prop_compose! {
    #[inline]
    pub fn arb_byte_array()(byte_array in vec(any::<u8>(), 1..=10)) -> ByteArray {
        ByteArray::new(byte_array)
    }
}

impl Arbitrary for ByteArray {
    type Parameters = ();
    #[inline]
    fn arbitrary_with(_args: ()) -> Self::Strategy {
        arb_byte_array().boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl WriteOp {
    pub fn value_strategy() -> impl Strategy<Value = Self> {
        vec(any::<u8>(), 0..64).prop_map(WriteOp::Value)
    }

    pub fn deletion_strategy() -> impl Strategy<Value = Self> {
        Just(WriteOp::Deletion)
    }
}

impl Arbitrary for WriteOp {
    type Parameters = ();
    fn arbitrary_with(_args: ()) -> Self::Strategy {
        prop_oneof![Self::deletion_strategy(), Self::value_strategy()].boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl WriteSet {
    fn genesis_strategy() -> impl Strategy<Value = Self> {
        vec((any::<AccessPath>(), WriteOp::value_strategy()), 0..64).prop_map(|write_set| {
            let write_set_mut = WriteSetMut::new(write_set);
            write_set_mut
                .freeze()
                .expect("generated write sets should always be valid")
        })
    }
}

impl Arbitrary for WriteSet {
    type Parameters = ();
    fn arbitrary_with(_args: ()) -> Self::Strategy {
        // XXX there's no checking for repeated access paths here, nor in write_set. Is that
        // important? Not sure.
        vec((any::<AccessPath>(), any::<WriteOp>()), 0..64)
            .prop_map(|write_set| {
                let write_set_mut = WriteSetMut::new(write_set);
                write_set_mut
                    .freeze()
                    .expect("generated write sets should always be valid")
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

#[derive(Debug)]
struct AccountInfo {
    address: AccountAddress,
    private_key: Ed25519PrivateKey,
    public_key: Ed25519PublicKey,
    sequence_number: u64,
    sent_event_handle: EventHandle,
    received_event_handle: EventHandle,
}

impl AccountInfo {
    pub fn new(private_key: Ed25519PrivateKey, public_key: Ed25519PublicKey) -> Self {
        let address = AccountAddress::from_public_key(&public_key);
        Self {
            address,
            private_key,
            public_key,
            sequence_number: 0,
            sent_event_handle: EventHandle::new_from_address(&address, 0),
            received_event_handle: EventHandle::new_from_address(&address, 1),
        }
    }
}

#[derive(Debug)]
pub struct AccountInfoUniverse {
    accounts: Vec<AccountInfo>,
}

impl AccountInfoUniverse {
    fn new(keypairs: Vec<(Ed25519PrivateKey, Ed25519PublicKey)>) -> Self {
        let accounts = keypairs
            .into_iter()
            .map(|(private_key, public_key)| AccountInfo::new(private_key, public_key))
            .collect();

        Self { accounts }
    }

    fn get_account_info(&self, account_index: Index) -> &AccountInfo {
        account_index.get(&self.accounts)
    }

    fn get_account_info_mut(&mut self, account_index: Index) -> &mut AccountInfo {
        account_index.get_mut(self.accounts.as_mut_slice())
    }
}

impl Arbitrary for AccountInfoUniverse {
    type Parameters = usize;
    fn arbitrary_with(num_accounts: Self::Parameters) -> Self::Strategy {
        vec(keypair_strategy(), num_accounts)
            .prop_map(Self::new)
            .boxed()
    }

    fn arbitrary() -> Self::Strategy {
        unimplemented!("Size of the universe must be provided explicitly (use any_with instead).")
    }

    type Strategy = BoxedStrategy<Self>;
}

#[derive(Arbitrary, Debug)]
pub struct RawTransactionGen {
    payload: TransactionPayload,
    max_gas_amount: u64,
    gas_unit_price: u64,
    expiration_time_secs: u64,
}

impl RawTransactionGen {
    pub fn materialize(
        self,
        sender_index: Index,
        universe: &mut AccountInfoUniverse,
    ) -> RawTransaction {
        let mut sender_info = universe.get_account_info_mut(sender_index);

        let sequence_number = sender_info.sequence_number;
        sender_info.sequence_number += 1;

        new_raw_transaction(
            sender_info.address,
            sequence_number,
            self.payload,
            self.max_gas_amount,
            self.gas_unit_price,
            self.expiration_time_secs,
        )
    }
}

impl RawTransaction {
    fn strategy_impl(
        address_strategy: impl Strategy<Value = AccountAddress>,
        payload_strategy: impl Strategy<Value = TransactionPayload>,
    ) -> impl Strategy<Value = Self> {
        // XXX what other constraints do these need to obey?
        (
            address_strategy,
            any::<u64>(),
            payload_strategy,
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
        )
            .prop_map(
                |(
                    sender,
                    sequence_number,
                    payload,
                    max_gas_amount,
                    gas_unit_price,
                    expiration_time_secs,
                )| {
                    new_raw_transaction(
                        sender,
                        sequence_number,
                        payload,
                        max_gas_amount,
                        gas_unit_price,
                        expiration_time_secs,
                    )
                },
            )
    }
}

fn new_raw_transaction(
    sender: AccountAddress,
    sequence_number: u64,
    payload: TransactionPayload,
    max_gas_amount: u64,
    gas_unit_price: u64,
    expiration_time_secs: u64,
) -> RawTransaction {
    match payload {
        TransactionPayload::Program(program) => RawTransaction::new(
            sender,
            sequence_number,
            TransactionPayload::Program(program),
            max_gas_amount,
            gas_unit_price,
            Duration::from_secs(expiration_time_secs),
        ),
        TransactionPayload::Module(module) => RawTransaction::new_module(
            sender,
            sequence_number,
            module,
            max_gas_amount,
            gas_unit_price,
            Duration::from_secs(expiration_time_secs),
        ),
        TransactionPayload::Script(script) => RawTransaction::new_script(
            sender,
            sequence_number,
            script,
            max_gas_amount,
            gas_unit_price,
            Duration::from_secs(expiration_time_secs),
        ),
        TransactionPayload::WriteSet(write_set) => {
            // It's a bit unfortunate that max_gas_amount etc is generated but
            // not used, but it isn't a huge deal.
            RawTransaction::new_write_set(sender, sequence_number, write_set)
        }
    }
}

impl Arbitrary for RawTransaction {
    type Parameters = ();
    fn arbitrary_with(_args: ()) -> Self::Strategy {
        Self::strategy_impl(any::<AccountAddress>(), any::<TransactionPayload>()).boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl SignatureCheckedTransaction {
    // This isn't an Arbitrary impl because this doesn't generate *any* possible SignedTransaction,
    // just one kind of them.
    pub fn program_strategy(
        keypair_strategy: impl Strategy<Value = (Ed25519PrivateKey, Ed25519PublicKey)>,
    ) -> impl Strategy<Value = Self> {
        Self::strategy_impl(keypair_strategy, TransactionPayload::program_strategy())
    }

    pub fn script_strategy(
        keypair_strategy: impl Strategy<Value = (Ed25519PrivateKey, Ed25519PublicKey)>,
    ) -> impl Strategy<Value = Self> {
        Self::strategy_impl(keypair_strategy, TransactionPayload::script_strategy())
    }

    pub fn module_strategy(
        keypair_strategy: impl Strategy<Value = (Ed25519PrivateKey, Ed25519PublicKey)>,
    ) -> impl Strategy<Value = Self> {
        Self::strategy_impl(keypair_strategy, TransactionPayload::module_strategy())
    }

    pub fn write_set_strategy(
        keypair_strategy: impl Strategy<Value = (Ed25519PrivateKey, Ed25519PublicKey)>,
    ) -> impl Strategy<Value = Self> {
        Self::strategy_impl(keypair_strategy, TransactionPayload::write_set_strategy())
    }

    pub fn genesis_strategy(
        keypair_strategy: impl Strategy<Value = (Ed25519PrivateKey, Ed25519PublicKey)>,
    ) -> impl Strategy<Value = Self> {
        Self::strategy_impl(keypair_strategy, TransactionPayload::genesis_strategy())
    }

    fn strategy_impl(
        keypair_strategy: impl Strategy<Value = (Ed25519PrivateKey, Ed25519PublicKey)>,
        payload_strategy: impl Strategy<Value = TransactionPayload>,
    ) -> impl Strategy<Value = Self> {
        (keypair_strategy, payload_strategy)
            .prop_flat_map(|(keypair, payload)| {
                let address = AccountAddress::from_public_key(&keypair.1);
                (
                    Just(keypair),
                    RawTransaction::strategy_impl(Just(address), Just(payload)),
                )
            })
            .prop_map(|((private_key, public_key), raw_txn)| {
                raw_txn
                    .sign(&private_key, public_key)
                    .expect("signing should always work")
            })
    }
}

#[derive(Arbitrary, Debug)]
pub struct SignatureCheckedTransactionGen {
    raw_transaction_gen: RawTransactionGen,
}

impl SignatureCheckedTransactionGen {
    pub fn materialize(
        self,
        sender_index: Index,
        universe: &mut AccountInfoUniverse,
    ) -> SignatureCheckedTransaction {
        let raw_txn = self.raw_transaction_gen.materialize(sender_index, universe);
        let account_info = universe.get_account_info(sender_index);
        raw_txn
            .sign(&account_info.private_key, account_info.public_key.clone())
            .expect("Signing raw transaction should work.")
    }
}

impl Arbitrary for SignatureCheckedTransaction {
    type Parameters = ();
    fn arbitrary_with(_args: ()) -> Self::Strategy {
        Self::strategy_impl(keypair_strategy(), any::<TransactionPayload>()).boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

/// This `Arbitrary` impl only generates valid signed transactions. TODO: maybe add invalid ones?
impl Arbitrary for SignedTransaction {
    type Parameters = ();
    fn arbitrary_with(_args: ()) -> Self::Strategy {
        any::<SignatureCheckedTransaction>()
            .prop_map(|txn| txn.into_inner())
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl TransactionPayload {
    pub fn program_strategy() -> impl Strategy<Value = Self> {
        any::<Program>().prop_map(TransactionPayload::Program)
    }

    pub fn script_strategy() -> impl Strategy<Value = Self> {
        any::<Script>().prop_map(TransactionPayload::Script)
    }

    pub fn module_strategy() -> impl Strategy<Value = Self> {
        any::<Module>().prop_map(TransactionPayload::Module)
    }

    pub fn write_set_strategy() -> impl Strategy<Value = Self> {
        any::<WriteSet>().prop_map(TransactionPayload::WriteSet)
    }

    /// Similar to `write_set_strategy` except generates a valid write set for the genesis block.
    pub fn genesis_strategy() -> impl Strategy<Value = Self> {
        WriteSet::genesis_strategy().prop_map(TransactionPayload::WriteSet)
    }
}

/// The `Arbitrary` impl only generates validation statuses since the full enum is too large.
impl Arbitrary for StatusCode {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(_args: ()) -> Self::Strategy {
        prop_oneof![
            Just(StatusCode::UNKNOWN_VALIDATION_STATUS),
            Just(StatusCode::INVALID_SIGNATURE),
            Just(StatusCode::INVALID_AUTH_KEY),
            Just(StatusCode::SEQUENCE_NUMBER_TOO_OLD),
            Just(StatusCode::SEQUENCE_NUMBER_TOO_NEW),
            Just(StatusCode::INSUFFICIENT_BALANCE_FOR_TRANSACTION_FEE),
            Just(StatusCode::TRANSACTION_EXPIRED),
            Just(StatusCode::SENDING_ACCOUNT_DOES_NOT_EXIST),
            Just(StatusCode::REJECTED_WRITE_SET),
            Just(StatusCode::INVALID_WRITE_SET),
            Just(StatusCode::EXCEEDED_MAX_TRANSACTION_SIZE),
            Just(StatusCode::UNKNOWN_SCRIPT),
            Just(StatusCode::UNKNOWN_MODULE),
            Just(StatusCode::MAX_GAS_UNITS_EXCEEDS_MAX_GAS_UNITS_BOUND),
            Just(StatusCode::MAX_GAS_UNITS_BELOW_MIN_TRANSACTION_GAS_UNITS),
            Just(StatusCode::GAS_UNIT_PRICE_BELOW_MIN_BOUND),
            Just(StatusCode::GAS_UNIT_PRICE_ABOVE_MAX_BOUND),
        ]
        .boxed()
    }
}

prop_compose! {
    fn arb_transaction_status()(vm_status in any::<VMStatus>()) -> TransactionStatus {
        vm_status.into()
    }
}

impl Arbitrary for TransactionStatus {
    type Parameters = ();
    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        arb_transaction_status().boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for TransactionPayload {
    type Parameters = ();
    fn arbitrary_with(_args: ()) -> Self::Strategy {
        // Most transactions in practice will be programs, but other parts of the system should
        // at least not choke on write set strategies so introduce them with decent probability.
        // The figures below are probability weights.
        prop_oneof![
            4 => Self::program_strategy(),
            4 => Self::script_strategy(),
            1 => Self::module_strategy(),
            1 => Self::write_set_strategy(),
        ]
        .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for Program {
    type Parameters = ();
    fn arbitrary_with(_args: ()) -> Self::Strategy {
        // XXX This should eventually be an actually valid program, maybe?
        // How should we generate random modules?
        // The vector sizes are picked out of thin air.
        (
            vec(any::<u8>(), 0..100),
            vec(any::<Vec<u8>>(), 0..100),
            vec(any::<TransactionArgument>(), 0..10),
        )
            .prop_map(|(code, modules, args)| Program::new(code, modules, args))
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for Script {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(_args: ()) -> Self::Strategy {
        // XXX This should eventually be an actually valid program, maybe?
        // The vector sizes are picked out of thin air.
        (
            vec(any::<u8>(), 0..100),
            vec(any::<TransactionArgument>(), 0..10),
        )
            .prop_map(|(code, args)| Script::new(code, args))
            .boxed()
    }
}

impl Arbitrary for Module {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(_args: ()) -> Self::Strategy {
        // XXX How should we generate random modules?
        // The vector sizes are picked out of thin air.
        vec(any::<u8>(), 0..100).prop_map(Module::new).boxed()
    }
}

impl Arbitrary for TransactionArgument {
    type Parameters = ();
    fn arbitrary_with(_args: ()) -> Self::Strategy {
        prop_oneof![
            any::<u64>().prop_map(TransactionArgument::U64),
            any::<AccountAddress>().prop_map(TransactionArgument::Address),
            any::<ByteArray>().prop_map(TransactionArgument::ByteArray),
            ".*".prop_map(TransactionArgument::String),
        ]
        .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

prop_compose! {
    fn arb_validator_signature_for_hash(hash: HashValue)(
        hash in Just(hash),
        (private_key, public_key) in keypair_strategy(),
    ) -> (AccountAddress, Ed25519Signature) {
        let signature = private_key.sign_message(&hash);
        (AccountAddress::from_public_key(&public_key), signature)
    }
}

impl Arbitrary for LedgerInfoWithSignatures<Ed25519Signature> {
    type Parameters = SizeRange;
    fn arbitrary_with(num_validators_range: Self::Parameters) -> Self::Strategy {
        (any::<LedgerInfo>(), Just(num_validators_range))
            .prop_flat_map(|(ledger_info, num_validators_range)| {
                let hash = ledger_info.hash();
                (
                    Just(ledger_info),
                    prop::collection::vec(
                        arb_validator_signature_for_hash(hash),
                        num_validators_range,
                    ),
                )
            })
            .prop_map(|(ledger_info, signatures)| {
                LedgerInfoWithSignatures::new(ledger_info, signatures.into_iter().collect())
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

prop_compose! {
    fn arb_update_to_latest_ledger_response()(
        response_items in vec(any::<ResponseItem>(), 0..10),
        ledger_info_with_sigs in any::<LedgerInfoWithSignatures<Ed25519Signature>>(),
        validator_change_events in vec(any::<ValidatorChangeEventWithProof<Ed25519Signature>>(), 0..10),
    ) -> UpdateToLatestLedgerResponse<Ed25519Signature> {
        UpdateToLatestLedgerResponse::new(
            response_items, ledger_info_with_sigs, validator_change_events)
    }
}

impl Arbitrary for UpdateToLatestLedgerResponse<Ed25519Signature> {
    type Parameters = ();
    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        arb_update_to_latest_ledger_response().boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

#[derive(Arbitrary, Debug)]
pub struct ContractEventGen {
    payload: Vec<u8>,
    use_sent_key: bool,
}

impl ContractEventGen {
    pub fn materialize(
        self,
        account_index: Index,
        universe: &mut AccountInfoUniverse,
    ) -> ContractEvent {
        let account_info = universe.get_account_info_mut(account_index);
        let event_handle = if self.use_sent_key {
            &mut account_info.sent_event_handle
        } else {
            &mut account_info.received_event_handle
        };
        let sequence_number = event_handle.count();
        *event_handle.count_mut() += 1;
        let event_key = event_handle.key();

        ContractEvent::new(*event_key, sequence_number, self.payload)
    }
}

#[derive(Arbitrary, Debug)]
struct AccountResourceGen {
    balance: u64,
    delegated_key_rotation_capability: bool,
    delegated_withdrawal_capability: bool,
}

impl AccountResourceGen {
    pub fn materialize(
        self,
        account_index: Index,
        universe: &AccountInfoUniverse,
    ) -> AccountResource {
        let account_info = universe.get_account_info(account_index);

        AccountResource::new(
            self.balance,
            account_info.sequence_number,
            ByteArray::new(account_info.public_key.to_bytes().to_vec()),
            self.delegated_key_rotation_capability,
            self.delegated_withdrawal_capability,
            account_info.sent_event_handle.clone(),
            account_info.received_event_handle.clone(),
        )
    }
}

#[derive(Arbitrary, Debug)]
struct AccountStateBlobGen {
    account_resource_gen: AccountResourceGen,
}

impl AccountStateBlobGen {
    pub fn materialize(
        self,
        account_index: Index,
        universe: &AccountInfoUniverse,
    ) -> AccountStateBlob {
        let account_resource = self
            .account_resource_gen
            .materialize(account_index, universe);
        AccountStateBlob::from(account_resource)
    }
}

impl ContractEvent {
    pub fn strategy_impl(
        event_key_strategy: impl Strategy<Value = EventKey>,
    ) -> impl Strategy<Value = Self> {
        (event_key_strategy, any::<u64>(), vec(any::<u8>(), 1..10)).prop_map(
            |(event_key, seq_num, event_data)| ContractEvent::new(event_key, seq_num, event_data),
        )
    }
}

impl EventHandle {
    pub fn strategy_impl(
        event_key_strategy: impl Strategy<Value = EventKey>,
    ) -> impl Strategy<Value = Self> {
        // We only generate small counters so that it won't overflow.
        (event_key_strategy, 0..std::u64::MAX / 2)
            .prop_map(|(event_key, counter)| EventHandle::new(event_key, counter))
    }
}

impl Arbitrary for EventHandle {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        EventHandle::strategy_impl(any::<EventKey>()).boxed()
    }
}

impl Arbitrary for ContractEvent {
    type Parameters = ();
    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        ContractEvent::strategy_impl(any::<EventKey>()).boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for TransactionToCommit {
    type Parameters = ();
    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (
            any_with::<AccountInfoUniverse>(1),
            any::<TransactionToCommitGen>(),
        )
            .prop_map(|(mut universe, gen)| gen.materialize(&mut universe))
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

/// Represents information already determined for generating a `TransactionToCommit`, along with
/// to be determined information that needs to settle upon `materialize()`, for example a to be
/// determined account can be represented by an `Index` which will be materialized to an entry in
/// the `AccountInfoUniverse`.
///
/// See `TransactionToCommitGen::materialize()` and supporting types.
#[derive(Debug)]
pub struct TransactionToCommitGen {
    /// Transaction sender and the transaction itself.
    transaction_gen: (Index, SignatureCheckedTransactionGen),
    /// Events: account and event content.
    event_gens: Vec<(Index, ContractEventGen)>,
    /// State updates: account and the blob.
    /// N.B. the transaction sender and event owners must be updated to reflect information such as
    /// sequence numbers so that test data generated through this is more realistic and logical.
    account_state_gens: Vec<(Index, AccountStateBlobGen)>,
    /// Gas used.
    gas_used: u64,
    /// Transaction status
    major_status: StatusCode,
}

impl TransactionToCommitGen {
    /// Materialize considering current states in the universe.
    pub fn materialize(self, universe: &mut AccountInfoUniverse) -> TransactionToCommit {
        let (sender_index, txn_gen) = self.transaction_gen;
        let signed_txn = txn_gen.materialize(sender_index, universe).into_inner();

        let events = self
            .event_gens
            .into_iter()
            .map(|(index, event_gen)| event_gen.materialize(index, universe))
            .collect();
        // Account states must be materialized last, to reflect the latest account and event
        // sequence numbers.
        let account_states = self
            .account_state_gens
            .into_iter()
            .map(|(index, blob_gen)| {
                (
                    universe.get_account_info(index).address,
                    blob_gen.materialize(index, universe),
                )
            })
            .collect();

        TransactionToCommit::new(
            signed_txn,
            account_states,
            events,
            self.gas_used,
            self.major_status,
        )
    }
}

impl Arbitrary for TransactionToCommitGen {
    type Parameters = ();

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (
            (
                any::<Index>(),
                any::<AccountStateBlobGen>(),
                any::<SignatureCheckedTransactionGen>(),
            ),
            vec(
                (
                    any::<Index>(),
                    any::<AccountStateBlobGen>(),
                    any::<ContractEventGen>(),
                ),
                0..=2,
            ),
            vec((any::<Index>(), any::<AccountStateBlobGen>()), 0..=1),
            any::<u64>(),
            any::<StatusCode>(),
        )
            .prop_map(
                |(sender, event_emitters, mut touched_accounts, gas_used, major_status)| {
                    // To reflect change of account/event sequence numbers, txn sender account and
                    // event emitter accounts must be updated.
                    let (sender_index, sender_blob_gen, txn_gen) = sender;
                    touched_accounts.push((sender_index, sender_blob_gen));

                    let mut event_gens = Vec::new();
                    for (index, blob_gen, event_gen) in event_emitters {
                        touched_accounts.push((index, blob_gen));
                        event_gens.push((index, event_gen));
                    }

                    Self {
                        transaction_gen: (sender_index, txn_gen),
                        event_gens,
                        account_state_gens: touched_accounts,
                        gas_used,
                        major_status,
                    }
                },
            )
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

fn arb_transaction_list_with_proof() -> impl Strategy<Value = TransactionListWithProof> {
    vec(
        (
            any::<SignedTransaction>(),
            any::<TransactionInfo>(),
            vec(any::<ContractEvent>(), 0..10),
        ),
        0..10,
    )
    .prop_flat_map(|transaction_and_infos_and_events| {
        let transaction_and_infos: Vec<_> = transaction_and_infos_and_events
            .clone()
            .into_iter()
            .map(|(transaction, info, _event)| (transaction, info))
            .collect();
        let events: Vec<_> = transaction_and_infos_and_events
            .into_iter()
            .map(|(_transaction, _info, event)| event)
            .collect();

        (
            Just(transaction_and_infos),
            option::of(Just(events)),
            any::<Version>(),
            any::<AccumulatorProof>(),
            any::<AccumulatorProof>(),
        )
    })
    .prop_map(
        |(
            transaction_and_infos,
            events,
            first_txn_version,
            proof_of_first_txn,
            proof_of_last_txn,
        )| {
            match transaction_and_infos.len() {
                0 => TransactionListWithProof::new_empty(),
                1 => TransactionListWithProof::new(
                    transaction_and_infos,
                    events,
                    Some(first_txn_version),
                    Some(proof_of_first_txn),
                    None,
                ),
                _ => TransactionListWithProof::new(
                    transaction_and_infos,
                    events,
                    Some(first_txn_version),
                    Some(proof_of_first_txn),
                    Some(proof_of_last_txn),
                ),
            }
        },
    )
}

impl Arbitrary for TransactionListWithProof {
    type Parameters = ();
    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        arb_transaction_list_with_proof().boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}
