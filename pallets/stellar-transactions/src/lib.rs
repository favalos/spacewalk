#![cfg_attr(not(feature = "std"), no_std)]
#![feature(result_flattening)]

extern crate alloc;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

mod currency;
mod horizon;

use codec::{Decode, Encode};
use frame_system::offchain::{SignedPayload, SigningTypes};
use orml_traits::MultiCurrency;
pub use pallet::*;
use pallet_transaction_payment::Config as PaymentConfig;
use sp_runtime::traits::{Convert, StaticLookup};
use sp_runtime::RuntimeDebug;
use sp_std::{convert::From, prelude::*, str};

use substrate_stellar_sdk as stellar;

use frame_support::pallet_prelude::*;
use frame_system::pallet_prelude::*;

type BalanceOf<T> = <<T as Config>::Currency as orml_traits::MultiCurrency<
    <T as frame_system::Config>::AccountId,
>>::Balance;

type CurrencyIdOf<T> =
    <<T as Config>::Currency as MultiCurrency<<T as frame_system::Config>::AccountId>>::CurrencyId;

#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug, scale_info::TypeInfo)]
pub struct DepositPayload<Currency, AccountId, Public, Balance> {
    currency_id: Currency,
    amount: Balance,
    destination: AccountId,
    signed_by: Public,
}

impl<T: SigningTypes> SignedPayload<T>
    for DepositPayload<
        CurrencyIdOf<T>,
        <T as frame_system::Config>::AccountId,
        T::Public,
        BalanceOf<T>,
    >
where
    T: pallet::Config,
{
    fn public(&self) -> T::Public {
        self.signed_by.clone()
    }
}

#[frame_support::pallet]
pub mod pallet {
    use super::*;
    use frame_support::dispatch::DispatchResultWithPostInfo;
    use frame_system::offchain::{
        AppCrypto, CreateSignedTransaction, SendUnsignedTransaction, Signer,
    };
    use stellar::{
        types::{OperationBody, PaymentOp},
        SecretKey, XdrCodec,
    };

    #[pallet::config]
    pub trait Config:
        frame_system::Config
        + CreateSignedTransaction<Call<Self>>
        + PaymentConfig
        + orml_tokens::Config
    {
        type AuthorityId: AppCrypto<Self::Public, Self::Signature>;
        /// The overarching dispatch call type.
        type Call: From<Call<Self>>;
        /// The overarching event type.
        type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

        /// The mechanics of the ORML tokens
        type Currency: MultiCurrency<<Self as frame_system::Config>::AccountId>;
        type AddressConversion: StaticLookup<
            Source = <Self as frame_system::Config>::AccountId,
            Target = substrate_stellar_sdk::PublicKey,
        >;
        type BalanceConversion: StaticLookup<Source = BalanceOf<Self>, Target = i64>;
        type StringCurrencyConversion: Convert<(Vec<u8>, Vec<u8>), Result<CurrencyIdOf<Self>, ()>>;

        /// Conversion between Stellar asset type and this pallet trait for Currency
        type CurrencyConversion: StaticLookup<
            Source = CurrencyIdOf<Self>,
            Target = substrate_stellar_sdk::Asset,
        >;

        /// The escrow account
        type GatewayEscrowKeypair: Get<SecretKey>;
    }

    #[pallet::pallet]
    #[pallet::generate_store(pub(super) trait Store)]
    pub struct Pallet<T>(_);

    #[pallet::event]
    #[pallet::generate_deposit(pub(super) fn deposit_event)]
    pub enum Event<T: Config> {
        /// Event generated when a new deposit is made on Stellar Escrow Account.
        Deposit(
            CurrencyIdOf<T>,
            <T as frame_system::Config>::AccountId,
            BalanceOf<T>,
        ),
    }

    #[pallet::error]
    pub enum Error<T> {
        // Error returned when making signed transactions in off-chain worker
        NoLocalAcctForSigning,

        // Error returned when making unsigned transactions with signed payloads in off-chain worker
        OffchainUnsignedTxSignedPayloadError,

        // XDR encoding/decoding error
        XdrCodecError,
    }

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        // TODO Benchmakr weights
        #[pallet::weight(10_000)]
        pub fn report_stellar_transaction(
            origin: OriginFor<T>,
            transaction_envelope_xdr: Vec<u8>,
        ) -> DispatchResult {
            let _who = ensure_signed(origin)?;

            let tx_xdr = base64::decode(&transaction_envelope_xdr).unwrap();
            let tx_envelope =
                substrate_stellar_sdk::TransactionEnvelope::from_xdr(&tx_xdr).unwrap();

            if let substrate_stellar_sdk::TransactionEnvelope::EnvelopeTypeTx(env) = tx_envelope {
                Self::process_new_transaction(env.tx);
            }
            Ok(())
        }

        // TODO Benchmark weight
        #[pallet::weight(10_000)]
        pub fn submit_deposit_unsigned_signed_payload(
            origin: OriginFor<T>,
            payload: DepositPayload<
                CurrencyIdOf<T>,
                <T as frame_system::Config>::AccountId,
                T::Public,
                BalanceOf<T>,
            >,
            _signature: T::Signature,
        ) -> DispatchResultWithPostInfo {
            let _ = ensure_none(origin)?;

            let DepositPayload {
                currency_id,
                amount,
                destination,
                signed_by,
            } = payload;

            log::info!(
                "Submit deposit: ({:?}, {:?}, {:?})",
                amount,
                destination,
                signed_by
            );

            let result = T::Currency::deposit(currency_id, &destination, amount);
            log::info!("{:?}", result);

            Self::deposit_event(Event::Deposit(currency_id, destination, amount));
            Ok(().into())
        }
    }

    impl<T: Config> Pallet<T> {
        fn process_new_transaction(transaction: stellar::types::Transaction) {
            // The destination of a mirrored Pendulum transaction, is always derived of the source
            // account that initiated the Stellar transaction.
            let destination = if let substrate_stellar_sdk::MuxedAccount::KeyTypeEd25519(key) =
                transaction.source_account
            {
                T::AddressConversion::unlookup(substrate_stellar_sdk::PublicKey::from_binary(key))
            } else {
                log::error!("❌  Source account format not supported.");
                return;
            };

            let payment_ops: Vec<&PaymentOp> = transaction
                .operations
                .get_vec()
                .into_iter()
                .filter_map(|op| match &op.body {
                    OperationBody::Payment(p) => Some(p),
                    _ => None,
                })
                .collect();

            for payment_op in payment_ops {
                let amount = T::BalanceConversion::unlookup(payment_op.amount);
                let currency = T::CurrencyConversion::unlookup(payment_op.asset.clone());

                match Self::send_payment_tx(currency, amount, destination.clone()) {
                    Err(_) => log::warn!("Sending the tx failed."),
                    Ok(_) => {
                        log::info!("✅ Deposit successfully Executed");
                        ()
                    }
                }
            }
        }

        fn send_payment_tx(
            currency_id: CurrencyIdOf<T>,
            deposit: BalanceOf<T>,
            destination: <T as frame_system::Config>::AccountId,
        ) -> Result<(), Error<T>> {
            let signer = Signer::<T, T::AuthorityId>::any_account();

            if let Some((_, res)) = signer.send_unsigned_transaction(
                |acct| DepositPayload {
                    currency_id,
                    amount: deposit,
                    destination: destination.clone(),
                    signed_by: acct.public.clone(),
                },
                |payload, signature| Call::submit_deposit_unsigned_signed_payload {
                    payload,
                    signature,
                },
            ) {
                return res.map_err(|_| {
                    log::error!("Failed in send_unsigned_tx_signed_payload");
                    Error::OffchainUnsignedTxSignedPayloadError
                });
            } else {
                // The case of `None`: no account is available for sending
                log::error!("No local account available");
                Err(Error::NoLocalAcctForSigning)
            }
        }
    }
}