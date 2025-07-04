use common_utils::{id_type, types::keymanager::KeyManagerState};
use diesel_models::{address::AddressUpdateInternal, enums::MerchantStorageScheme};
use error_stack::ResultExt;

use super::MockDb;
use crate::{
    core::errors::{self, CustomResult},
    types::{
        domain::{
            self,
            behaviour::{Conversion, ReverseConversion},
        },
        storage as storage_types,
    },
};

#[async_trait::async_trait]
pub trait AddressInterface
where
    domain::Address:
        Conversion<DstType = storage_types::Address, NewDstType = storage_types::AddressNew>,
{
    async fn update_address(
        &self,
        state: &KeyManagerState,
        address_id: String,
        address: storage_types::AddressUpdate,
        key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<domain::Address, errors::StorageError>;

    async fn update_address_for_payments(
        &self,
        state: &KeyManagerState,
        this: domain::PaymentAddress,
        address: domain::AddressUpdate,
        payment_id: id_type::PaymentId,
        key_store: &domain::MerchantKeyStore,
        storage_scheme: MerchantStorageScheme,
    ) -> CustomResult<domain::PaymentAddress, errors::StorageError>;

    async fn find_address_by_address_id(
        &self,
        state: &KeyManagerState,
        address_id: &str,
        key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<domain::Address, errors::StorageError>;

    async fn insert_address_for_payments(
        &self,
        state: &KeyManagerState,
        payment_id: &id_type::PaymentId,
        address: domain::PaymentAddress,
        key_store: &domain::MerchantKeyStore,
        storage_scheme: MerchantStorageScheme,
    ) -> CustomResult<domain::PaymentAddress, errors::StorageError>;

    async fn insert_address_for_customers(
        &self,
        state: &KeyManagerState,
        address: domain::CustomerAddress,
        key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<domain::Address, errors::StorageError>;

    async fn find_address_by_merchant_id_payment_id_address_id(
        &self,
        state: &KeyManagerState,
        merchant_id: &id_type::MerchantId,
        payment_id: &id_type::PaymentId,
        address_id: &str,
        key_store: &domain::MerchantKeyStore,
        storage_scheme: MerchantStorageScheme,
    ) -> CustomResult<domain::PaymentAddress, errors::StorageError>;

    async fn update_address_by_merchant_id_customer_id(
        &self,
        state: &KeyManagerState,
        customer_id: &id_type::CustomerId,
        merchant_id: &id_type::MerchantId,
        address: storage_types::AddressUpdate,
        key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<Vec<domain::Address>, errors::StorageError>;
}

#[cfg(not(feature = "kv_store"))]
mod storage {
    use common_utils::{ext_traits::AsyncExt, id_type, types::keymanager::KeyManagerState};
    use error_stack::{report, ResultExt};
    use router_env::{instrument, tracing};

    use super::AddressInterface;
    use crate::{
        connection,
        core::errors::{self, CustomResult},
        services::Store,
        types::{
            domain::{
                self,
                behaviour::{Conversion, ReverseConversion},
            },
            storage::{self as storage_types, enums::MerchantStorageScheme},
        },
    };
    #[async_trait::async_trait]
    impl AddressInterface for Store {
        #[instrument(skip_all)]
        async fn find_address_by_address_id(
            &self,
            state: &KeyManagerState,
            address_id: &str,
            key_store: &domain::MerchantKeyStore,
        ) -> CustomResult<domain::Address, errors::StorageError> {
            let conn = connection::pg_connection_read(self).await?;
            storage_types::Address::find_by_address_id(&conn, address_id)
                .await
                .map_err(|error| report!(errors::StorageError::from(error)))
                .async_and_then(|address| async {
                    address
                        .convert(
                            state,
                            key_store.key.get_inner(),
                            key_store.merchant_id.clone().into(),
                        )
                        .await
                        .change_context(errors::StorageError::DecryptionError)
                })
                .await
        }

        #[instrument(skip_all)]
        async fn find_address_by_merchant_id_payment_id_address_id(
            &self,
            state: &KeyManagerState,
            merchant_id: &id_type::MerchantId,
            payment_id: &id_type::PaymentId,
            address_id: &str,
            key_store: &domain::MerchantKeyStore,
            _storage_scheme: MerchantStorageScheme,
        ) -> CustomResult<domain::PaymentAddress, errors::StorageError> {
            let conn = connection::pg_connection_read(self).await?;
            storage_types::Address::find_by_merchant_id_payment_id_address_id(
                &conn,
                merchant_id,
                payment_id,
                address_id,
            )
            .await
            .map_err(|error| report!(errors::StorageError::from(error)))
            .async_and_then(|address| async {
                address
                    .convert(state, key_store.key.get_inner(), merchant_id.clone().into())
                    .await
                    .change_context(errors::StorageError::DecryptionError)
            })
            .await
        }

        #[instrument(skip_all)]
        async fn update_address(
            &self,
            state: &KeyManagerState,
            address_id: String,
            address: storage_types::AddressUpdate,
            key_store: &domain::MerchantKeyStore,
        ) -> CustomResult<domain::Address, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            storage_types::Address::update_by_address_id(&conn, address_id, address.into())
                .await
                .map_err(|error| report!(errors::StorageError::from(error)))
                .async_and_then(|address| async {
                    address
                        .convert(
                            state,
                            key_store.key.get_inner(),
                            key_store.merchant_id.clone().into(),
                        )
                        .await
                        .change_context(errors::StorageError::DecryptionError)
                })
                .await
        }

        #[instrument(skip_all)]
        async fn update_address_for_payments(
            &self,
            state: &KeyManagerState,
            this: domain::PaymentAddress,
            address_update: domain::AddressUpdate,
            _payment_id: id_type::PaymentId,
            key_store: &domain::MerchantKeyStore,
            _storage_scheme: MerchantStorageScheme,
        ) -> CustomResult<domain::PaymentAddress, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            let address = Conversion::convert(this)
                .await
                .change_context(errors::StorageError::EncryptionError)?;
            address
                .update(&conn, address_update.into())
                .await
                .map_err(|error| report!(errors::StorageError::from(error)))
                .async_and_then(|address| async {
                    address
                        .convert(
                            state,
                            key_store.key.get_inner(),
                            key_store.merchant_id.clone().into(),
                        )
                        .await
                        .change_context(errors::StorageError::DecryptionError)
                })
                .await
        }

        #[instrument(skip_all)]
        async fn insert_address_for_payments(
            &self,
            state: &KeyManagerState,
            _payment_id: &id_type::PaymentId,
            address: domain::PaymentAddress,
            key_store: &domain::MerchantKeyStore,
            _storage_scheme: MerchantStorageScheme,
        ) -> CustomResult<domain::PaymentAddress, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            address
                .construct_new()
                .await
                .change_context(errors::StorageError::EncryptionError)?
                .insert(&conn)
                .await
                .map_err(|error| report!(errors::StorageError::from(error)))
                .async_and_then(|address| async {
                    address
                        .convert(
                            state,
                            key_store.key.get_inner(),
                            key_store.merchant_id.clone().into(),
                        )
                        .await
                        .change_context(errors::StorageError::DecryptionError)
                })
                .await
        }

        #[instrument(skip_all)]
        async fn insert_address_for_customers(
            &self,
            state: &KeyManagerState,
            address: domain::CustomerAddress,
            key_store: &domain::MerchantKeyStore,
        ) -> CustomResult<domain::Address, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            address
                .construct_new()
                .await
                .change_context(errors::StorageError::EncryptionError)?
                .insert(&conn)
                .await
                .map_err(|error| report!(errors::StorageError::from(error)))
                .async_and_then(|address| async {
                    address
                        .convert(
                            state,
                            key_store.key.get_inner(),
                            key_store.merchant_id.clone().into(),
                        )
                        .await
                        .change_context(errors::StorageError::DecryptionError)
                })
                .await
        }

        #[instrument(skip_all)]
        async fn update_address_by_merchant_id_customer_id(
            &self,
            state: &KeyManagerState,
            customer_id: &id_type::CustomerId,
            merchant_id: &id_type::MerchantId,
            address: storage_types::AddressUpdate,
            key_store: &domain::MerchantKeyStore,
        ) -> CustomResult<Vec<domain::Address>, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            storage_types::Address::update_by_merchant_id_customer_id(
                &conn,
                customer_id,
                merchant_id,
                address.into(),
            )
            .await
            .map_err(|error| report!(errors::StorageError::from(error)))
            .async_and_then(|addresses| async {
                let mut output = Vec::with_capacity(addresses.len());
                for address in addresses.into_iter() {
                    output.push(
                        address
                            .convert(state, key_store.key.get_inner(), merchant_id.clone().into())
                            .await
                            .change_context(errors::StorageError::DecryptionError)?,
                    )
                }
                Ok(output)
            })
            .await
        }
    }
}

#[cfg(feature = "kv_store")]
mod storage {
    use common_utils::{ext_traits::AsyncExt, id_type, types::keymanager::KeyManagerState};
    use diesel_models::{enums::MerchantStorageScheme, AddressUpdateInternal};
    use error_stack::{report, ResultExt};
    use redis_interface::HsetnxReply;
    use router_env::{instrument, tracing};
    use storage_impl::redis::kv_store::{
        decide_storage_scheme, kv_wrapper, KvOperation, Op, PartitionKey,
    };

    use super::AddressInterface;
    use crate::{
        connection,
        core::errors::{self, CustomResult},
        services::Store,
        types::{
            domain::{
                self,
                behaviour::{Conversion, ReverseConversion},
            },
            storage::{self as storage_types, kv},
        },
        utils::db_utils,
    };
    #[async_trait::async_trait]
    impl AddressInterface for Store {
        #[instrument(skip_all)]
        async fn find_address_by_address_id(
            &self,
            state: &KeyManagerState,
            address_id: &str,
            key_store: &domain::MerchantKeyStore,
        ) -> CustomResult<domain::Address, errors::StorageError> {
            let conn = connection::pg_connection_read(self).await?;
            storage_types::Address::find_by_address_id(&conn, address_id)
                .await
                .map_err(|error| report!(errors::StorageError::from(error)))
                .async_and_then(|address| async {
                    address
                        .convert(
                            state,
                            key_store.key.get_inner(),
                            key_store.merchant_id.clone().into(),
                        )
                        .await
                        .change_context(errors::StorageError::DecryptionError)
                })
                .await
        }

        #[instrument(skip_all)]
        async fn find_address_by_merchant_id_payment_id_address_id(
            &self,
            state: &KeyManagerState,
            merchant_id: &id_type::MerchantId,
            payment_id: &id_type::PaymentId,
            address_id: &str,
            key_store: &domain::MerchantKeyStore,
            storage_scheme: MerchantStorageScheme,
        ) -> CustomResult<domain::PaymentAddress, errors::StorageError> {
            let conn = connection::pg_connection_read(self).await?;
            let database_call = || async {
                storage_types::Address::find_by_merchant_id_payment_id_address_id(
                    &conn,
                    merchant_id,
                    payment_id,
                    address_id,
                )
                .await
                .map_err(|error| report!(errors::StorageError::from(error)))
            };
            let storage_scheme = Box::pin(decide_storage_scheme::<_, storage_types::Address>(
                self,
                storage_scheme,
                Op::Find,
            ))
            .await;
            let address = match storage_scheme {
                MerchantStorageScheme::PostgresOnly => database_call().await,
                MerchantStorageScheme::RedisKv => {
                    let key = PartitionKey::MerchantIdPaymentId {
                        merchant_id,
                        payment_id,
                    };
                    let field = format!("add_{address_id}");
                    Box::pin(db_utils::try_redis_get_else_try_database_get(
                        async {
                            Box::pin(kv_wrapper(
                                self,
                                KvOperation::<diesel_models::Address>::HGet(&field),
                                key,
                            ))
                            .await?
                            .try_into_hget()
                        },
                        database_call,
                    ))
                    .await
                }
            }?;
            address
                .convert(
                    state,
                    key_store.key.get_inner(),
                    common_utils::types::keymanager::Identifier::Merchant(
                        key_store.merchant_id.clone(),
                    ),
                )
                .await
                .change_context(errors::StorageError::DecryptionError)
        }

        #[instrument(skip_all)]
        async fn update_address(
            &self,
            state: &KeyManagerState,
            address_id: String,
            address: storage_types::AddressUpdate,
            key_store: &domain::MerchantKeyStore,
        ) -> CustomResult<domain::Address, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            storage_types::Address::update_by_address_id(&conn, address_id, address.into())
                .await
                .map_err(|error| report!(errors::StorageError::from(error)))
                .async_and_then(|address| async {
                    address
                        .convert(
                            state,
                            key_store.key.get_inner(),
                            key_store.merchant_id.clone().into(),
                        )
                        .await
                        .change_context(errors::StorageError::DecryptionError)
                })
                .await
        }

        #[instrument(skip_all)]
        async fn update_address_for_payments(
            &self,
            state: &KeyManagerState,
            this: domain::PaymentAddress,
            address_update: domain::AddressUpdate,
            payment_id: id_type::PaymentId,
            key_store: &domain::MerchantKeyStore,
            storage_scheme: MerchantStorageScheme,
        ) -> CustomResult<domain::PaymentAddress, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            let address = Conversion::convert(this)
                .await
                .change_context(errors::StorageError::EncryptionError)?;
            let merchant_id = address.merchant_id.clone();
            let key = PartitionKey::MerchantIdPaymentId {
                merchant_id: &merchant_id,
                payment_id: &payment_id,
            };
            let field = format!("add_{}", address.address_id);
            let storage_scheme = Box::pin(decide_storage_scheme::<_, storage_types::Address>(
                self,
                storage_scheme,
                Op::Update(key.clone(), &field, Some(address.updated_by.as_str())),
            ))
            .await;
            match storage_scheme {
                MerchantStorageScheme::PostgresOnly => {
                    address
                        .update(&conn, address_update.into())
                        .await
                        .map_err(|error| report!(errors::StorageError::from(error)))
                        .async_and_then(|address| async {
                            address
                                .convert(
                                    state,
                                    key_store.key.get_inner(),
                                    key_store.merchant_id.clone().into(),
                                )
                                .await
                                .change_context(errors::StorageError::DecryptionError)
                        })
                        .await
                }
                MerchantStorageScheme::RedisKv => {
                    let updated_address = AddressUpdateInternal::from(address_update.clone())
                        .create_address(address.clone());
                    let redis_value = serde_json::to_string(&updated_address)
                        .change_context(errors::StorageError::KVError)?;

                    let redis_entry = kv::TypedSql {
                        op: kv::DBOperation::Update {
                            updatable: Box::new(kv::Updateable::AddressUpdate(Box::new(
                                kv::AddressUpdateMems {
                                    orig: address,
                                    update_data: address_update.into(),
                                },
                            ))),
                        },
                    };

                    Box::pin(kv_wrapper::<(), _, _>(
                        self,
                        KvOperation::Hset::<storage_types::Address>(
                            (&field, redis_value),
                            redis_entry,
                        ),
                        key,
                    ))
                    .await
                    .change_context(errors::StorageError::KVError)?
                    .try_into_hset()
                    .change_context(errors::StorageError::KVError)?;

                    updated_address
                        .convert(
                            state,
                            key_store.key.get_inner(),
                            key_store.merchant_id.clone().into(),
                        )
                        .await
                        .change_context(errors::StorageError::DecryptionError)
                }
            }
        }

        #[instrument(skip_all)]
        async fn insert_address_for_payments(
            &self,
            state: &KeyManagerState,
            payment_id: &id_type::PaymentId,
            address: domain::PaymentAddress,
            key_store: &domain::MerchantKeyStore,
            storage_scheme: MerchantStorageScheme,
        ) -> CustomResult<domain::PaymentAddress, errors::StorageError> {
            let address_new = address
                .clone()
                .construct_new()
                .await
                .change_context(errors::StorageError::EncryptionError)?;
            let merchant_id = address_new.merchant_id.clone();
            let storage_scheme = Box::pin(decide_storage_scheme::<_, storage_types::Address>(
                self,
                storage_scheme,
                Op::Insert,
            ))
            .await;
            match storage_scheme {
                MerchantStorageScheme::PostgresOnly => {
                    let conn = connection::pg_connection_write(self).await?;
                    address_new
                        .insert(&conn)
                        .await
                        .map_err(|error| report!(errors::StorageError::from(error)))
                        .async_and_then(|address| async {
                            address
                                .convert(
                                    state,
                                    key_store.key.get_inner(),
                                    key_store.merchant_id.clone().into(),
                                )
                                .await
                                .change_context(errors::StorageError::DecryptionError)
                        })
                        .await
                }
                MerchantStorageScheme::RedisKv => {
                    let key = PartitionKey::MerchantIdPaymentId {
                        merchant_id: &merchant_id,
                        payment_id,
                    };
                    let field = format!("add_{}", &address_new.address_id);
                    let created_address = diesel_models::Address {
                        address_id: address_new.address_id.clone(),
                        city: address_new.city.clone(),
                        country: address_new.country,
                        line1: address_new.line1.clone(),
                        line2: address_new.line2.clone(),
                        line3: address_new.line3.clone(),
                        state: address_new.state.clone(),
                        zip: address_new.zip.clone(),
                        first_name: address_new.first_name.clone(),
                        last_name: address_new.last_name.clone(),
                        phone_number: address_new.phone_number.clone(),
                        country_code: address_new.country_code.clone(),
                        created_at: address_new.created_at,
                        modified_at: address_new.modified_at,
                        customer_id: address_new.customer_id.clone(),
                        merchant_id: address_new.merchant_id.clone(),
                        payment_id: address_new.payment_id.clone(),
                        updated_by: storage_scheme.to_string(),
                        email: address_new.email.clone(),
                    };

                    let redis_entry = kv::TypedSql {
                        op: kv::DBOperation::Insert {
                            insertable: Box::new(kv::Insertable::Address(Box::new(address_new))),
                        },
                    };

                    match Box::pin(kv_wrapper::<diesel_models::Address, _, _>(
                        self,
                        KvOperation::HSetNx::<diesel_models::Address>(
                            &field,
                            &created_address,
                            redis_entry,
                        ),
                        key,
                    ))
                    .await
                    .change_context(errors::StorageError::KVError)?
                    .try_into_hsetnx()
                    {
                        Ok(HsetnxReply::KeyNotSet) => Err(errors::StorageError::DuplicateValue {
                            entity: "address",
                            key: Some(created_address.address_id),
                        }
                        .into()),
                        Ok(HsetnxReply::KeySet) => Ok(created_address
                            .convert(
                                state,
                                key_store.key.get_inner(),
                                key_store.merchant_id.clone().into(),
                            )
                            .await
                            .change_context(errors::StorageError::DecryptionError)?),
                        Err(er) => Err(er).change_context(errors::StorageError::KVError),
                    }
                }
            }
        }

        #[instrument(skip_all)]
        async fn insert_address_for_customers(
            &self,
            state: &KeyManagerState,
            address: domain::CustomerAddress,
            key_store: &domain::MerchantKeyStore,
        ) -> CustomResult<domain::Address, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            address
                .construct_new()
                .await
                .change_context(errors::StorageError::EncryptionError)?
                .insert(&conn)
                .await
                .map_err(|error| report!(errors::StorageError::from(error)))
                .async_and_then(|address| async {
                    address
                        .convert(
                            state,
                            key_store.key.get_inner(),
                            key_store.merchant_id.clone().into(),
                        )
                        .await
                        .change_context(errors::StorageError::DecryptionError)
                })
                .await
        }

        #[instrument(skip_all)]
        async fn update_address_by_merchant_id_customer_id(
            &self,
            state: &KeyManagerState,
            customer_id: &id_type::CustomerId,
            merchant_id: &id_type::MerchantId,
            address: storage_types::AddressUpdate,
            key_store: &domain::MerchantKeyStore,
        ) -> CustomResult<Vec<domain::Address>, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            storage_types::Address::update_by_merchant_id_customer_id(
                &conn,
                customer_id,
                merchant_id,
                address.into(),
            )
            .await
            .map_err(|error| report!(errors::StorageError::from(error)))
            .async_and_then(|addresses| async {
                let mut output = Vec::with_capacity(addresses.len());
                for address in addresses.into_iter() {
                    output.push(
                        address
                            .convert(
                                state,
                                key_store.key.get_inner(),
                                key_store.merchant_id.clone().into(),
                            )
                            .await
                            .change_context(errors::StorageError::DecryptionError)?,
                    )
                }
                Ok(output)
            })
            .await
        }
    }
}

#[async_trait::async_trait]
impl AddressInterface for MockDb {
    async fn find_address_by_address_id(
        &self,
        state: &KeyManagerState,
        address_id: &str,
        key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<domain::Address, errors::StorageError> {
        match self
            .addresses
            .lock()
            .await
            .iter()
            .find(|address| address.address_id == address_id)
        {
            Some(address) => address
                .clone()
                .convert(
                    state,
                    key_store.key.get_inner(),
                    key_store.merchant_id.clone().into(),
                )
                .await
                .change_context(errors::StorageError::DecryptionError),
            None => {
                return Err(
                    errors::StorageError::ValueNotFound("address not found".to_string()).into(),
                )
            }
        }
    }

    async fn find_address_by_merchant_id_payment_id_address_id(
        &self,
        state: &KeyManagerState,
        _merchant_id: &id_type::MerchantId,
        _payment_id: &id_type::PaymentId,
        address_id: &str,
        key_store: &domain::MerchantKeyStore,
        _storage_scheme: MerchantStorageScheme,
    ) -> CustomResult<domain::PaymentAddress, errors::StorageError> {
        match self
            .addresses
            .lock()
            .await
            .iter()
            .find(|address| address.address_id == address_id)
        {
            Some(address) => address
                .clone()
                .convert(
                    state,
                    key_store.key.get_inner(),
                    key_store.merchant_id.clone().into(),
                )
                .await
                .change_context(errors::StorageError::DecryptionError),
            None => {
                return Err(
                    errors::StorageError::ValueNotFound("address not found".to_string()).into(),
                )
            }
        }
    }

    async fn update_address(
        &self,
        state: &KeyManagerState,
        address_id: String,
        address_update: storage_types::AddressUpdate,
        key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<domain::Address, errors::StorageError> {
        let updated_addr = self
            .addresses
            .lock()
            .await
            .iter_mut()
            .find(|address| address.address_id == address_id)
            .map(|a| {
                let address_updated =
                    AddressUpdateInternal::from(address_update).create_address(a.clone());
                *a = address_updated.clone();
                address_updated
            });
        match updated_addr {
            Some(address_updated) => address_updated
                .convert(
                    state,
                    key_store.key.get_inner(),
                    key_store.merchant_id.clone().into(),
                )
                .await
                .change_context(errors::StorageError::DecryptionError),
            None => Err(errors::StorageError::ValueNotFound(
                "cannot find address to update".to_string(),
            )
            .into()),
        }
    }

    async fn update_address_for_payments(
        &self,
        state: &KeyManagerState,
        this: domain::PaymentAddress,
        address_update: domain::AddressUpdate,
        _payment_id: id_type::PaymentId,
        key_store: &domain::MerchantKeyStore,
        _storage_scheme: MerchantStorageScheme,
    ) -> CustomResult<domain::PaymentAddress, errors::StorageError> {
        let updated_addr = self
            .addresses
            .lock()
            .await
            .iter_mut()
            .find(|address| address.address_id == this.address.address_id)
            .map(|a| {
                let address_updated =
                    AddressUpdateInternal::from(address_update).create_address(a.clone());
                *a = address_updated.clone();
                address_updated
            });
        match updated_addr {
            Some(address_updated) => address_updated
                .convert(
                    state,
                    key_store.key.get_inner(),
                    key_store.merchant_id.clone().into(),
                )
                .await
                .change_context(errors::StorageError::DecryptionError),
            None => Err(errors::StorageError::ValueNotFound(
                "cannot find address to update".to_string(),
            )
            .into()),
        }
    }

    async fn insert_address_for_payments(
        &self,
        state: &KeyManagerState,
        _payment_id: &id_type::PaymentId,
        address_new: domain::PaymentAddress,
        key_store: &domain::MerchantKeyStore,
        _storage_scheme: MerchantStorageScheme,
    ) -> CustomResult<domain::PaymentAddress, errors::StorageError> {
        let mut addresses = self.addresses.lock().await;

        let address = Conversion::convert(address_new)
            .await
            .change_context(errors::StorageError::EncryptionError)?;

        addresses.push(address.clone());

        address
            .convert(
                state,
                key_store.key.get_inner(),
                key_store.merchant_id.clone().into(),
            )
            .await
            .change_context(errors::StorageError::DecryptionError)
    }

    async fn insert_address_for_customers(
        &self,
        state: &KeyManagerState,
        address_new: domain::CustomerAddress,
        key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<domain::Address, errors::StorageError> {
        let mut addresses = self.addresses.lock().await;

        let address = Conversion::convert(address_new)
            .await
            .change_context(errors::StorageError::EncryptionError)?;

        addresses.push(address.clone());

        address
            .convert(
                state,
                key_store.key.get_inner(),
                key_store.merchant_id.clone().into(),
            )
            .await
            .change_context(errors::StorageError::DecryptionError)
    }

    async fn update_address_by_merchant_id_customer_id(
        &self,
        state: &KeyManagerState,
        customer_id: &id_type::CustomerId,
        merchant_id: &id_type::MerchantId,
        address_update: storage_types::AddressUpdate,
        key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<Vec<domain::Address>, errors::StorageError> {
        let updated_addr = self
            .addresses
            .lock()
            .await
            .iter_mut()
            .find(|address| {
                address.customer_id.as_ref() == Some(customer_id)
                    && address.merchant_id == *merchant_id
            })
            .map(|a| {
                let address_updated =
                    AddressUpdateInternal::from(address_update).create_address(a.clone());
                *a = address_updated.clone();
                address_updated
            });
        match updated_addr {
            Some(address) => {
                let address: domain::Address = address
                    .convert(
                        state,
                        key_store.key.get_inner(),
                        key_store.merchant_id.clone().into(),
                    )
                    .await
                    .change_context(errors::StorageError::DecryptionError)?;
                Ok(vec![address])
            }
            None => {
                Err(errors::StorageError::ValueNotFound("address not found".to_string()).into())
            }
        }
    }
}
