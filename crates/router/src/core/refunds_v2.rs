use api_models::refunds::RefundErrorDetails;
use common_utils::{
    id_type,
    types::{ConnectorTransactionId, MinorUnit},
};
use error_stack::{report, ResultExt};
use hyperswitch_domain_models::router_data::ErrorResponse;
use hyperswitch_interfaces::integrity::{CheckIntegrity, FlowIntegrity, GetIntegrityObject};
use router_env::{instrument, tracing};

use crate::{
    consts,
    core::{
        errors::{self, ConnectorErrorExt, RouterResponse, RouterResult, StorageErrorExt},
        payments::{self, access_token, helpers},
        utils::{self as core_utils, refunds_validator},
    },
    db, logger,
    routes::{metrics, SessionState},
    services,
    types::{
        self,
        api::{self, refunds},
        domain,
        storage::{self, enums},
        transformers::{ForeignFrom, ForeignTryFrom},
    },
    utils,
};

#[instrument(skip_all)]
pub async fn refund_create_core(
    state: SessionState,
    merchant_account: domain::MerchantAccount,
    _profile: domain::Profile,
    key_store: domain::MerchantKeyStore,
    req: refunds::RefundsCreateRequest,
) -> RouterResponse<refunds::RefundResponse> {
    let db = &*state.store;
    let (merchant_id, payment_intent, payment_attempt, amount);

    merchant_id = merchant_account.get_id();

    payment_intent = db
        .find_payment_intent_by_id(
            &(&state).into(),
            &req.payment_id,
            &key_store,
            merchant_account.storage_scheme,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

    utils::when(
        !(payment_intent.status == enums::IntentStatus::Succeeded
            || payment_intent.status == enums::IntentStatus::PartiallyCaptured),
        || {
            Err(report!(errors::ApiErrorResponse::PaymentUnexpectedState {
                current_flow: "refund".into(),
                field_name: "status".into(),
                current_value: payment_intent.status.to_string(),
                states: "succeeded, partially_captured".to_string()
            })
            .attach_printable("unable to refund for a unsuccessful payment intent"))
        },
    )?;

    // Amount is not passed in request refer from payment intent.
    amount = req
        .amount
        .or(payment_intent.amount_captured)
        .ok_or(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("amount captured is none in a successful payment")?;

    utils::when(amount <= MinorUnit::new(0), || {
        Err(report!(errors::ApiErrorResponse::InvalidDataFormat {
            field_name: "amount".to_string(),
            expected_format: "positive integer".to_string()
        })
        .attach_printable("amount less than or equal to zero"))
    })?;

    payment_attempt = db
        .find_payment_attempt_last_successful_or_partially_captured_attempt_by_payment_id_merchant_id(
            &(&state).into(),
            &key_store,
            &req.payment_id,
            merchant_id,
            merchant_account.storage_scheme,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::SuccessfulPaymentNotFound)?;

    Box::pin(validate_and_create_refund(
        &state,
        &merchant_account,
        &key_store,
        &payment_attempt,
        &payment_intent,
        amount,
        req,
    ))
    .await
    .map(services::ApplicationResponse::Json)
}

#[allow(clippy::too_many_arguments)]
#[instrument(skip_all)]
pub async fn trigger_refund_to_gateway(
    state: &SessionState,
    refund: &storage::Refund,
    merchant_account: &domain::MerchantAccount,
    key_store: &domain::MerchantKeyStore,
    payment_attempt: &storage::PaymentAttempt,
    payment_intent: &storage::PaymentIntent,
) -> RouterResult<storage::Refund> {
    let db = &*state.store;

    let mca_id = payment_attempt
        .merchant_connector_id
        .clone()
        .ok_or(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to retrieve mca id from payment attempt")?;

    let storage_scheme = merchant_account.storage_scheme;
    metrics::REFUND_COUNT.add(
        1,
        router_env::metric_attributes!(("connector", mca_id.get_string_repr().to_string())),
    );

    let mca = db
        .find_merchant_connector_account_by_id(&state.into(), &mca_id, key_store)
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to fetch merchant connector account")?;

    let connector_name = mca.connector_name.to_string();

    let connector: api::ConnectorData = api::ConnectorData::get_connector_by_name(
        &state.conf.connectors,
        &connector_name,
        api::GetToken::Connector,
        Some(mca_id.clone()),
    )?;

    let currency = payment_intent.amount_details.currency;

    refunds_validator::validate_for_valid_refunds(payment_attempt, connector.connector_name)?;

    let mut router_data = core_utils::construct_refund_router_data(
        state,
        &connector_name,
        merchant_account,
        key_store,
        (payment_attempt.get_total_amount(), currency),
        payment_intent,
        payment_attempt,
        refund,
        &mca,
    )
    .await?;

    let add_access_token_result =
        access_token::add_access_token(state, &connector, merchant_account, &router_data, None)
            .await?;

    logger::debug!(refund_router_data=?router_data);

    access_token::update_router_data_with_access_token_result(
        &add_access_token_result,
        &mut router_data,
        &payments::CallConnectorAction::Trigger,
    );

    let router_data_res = if !(add_access_token_result.connector_supports_access_token
        && router_data.access_token.is_none())
    {
        let connector_integration: services::BoxedRefundConnectorIntegrationInterface<
            api::Execute,
            types::RefundsData,
            types::RefundsResponseData,
        > = connector.connector.get_connector_integration();
        let router_data_res = services::execute_connector_processing_step(
            state,
            connector_integration,
            &router_data,
            payments::CallConnectorAction::Trigger,
            None,
        )
        .await;
        let option_refund_error_update =
            router_data_res
                .as_ref()
                .err()
                .and_then(|error| match error.current_context() {
                    errors::ConnectorError::NotImplemented(message) => {
                        Some(storage::RefundUpdate::ErrorUpdate {
                            refund_status: Some(enums::RefundStatus::Failure),
                            refund_error_message: Some(
                                errors::ConnectorError::NotImplemented(message.to_owned())
                                    .to_string(),
                            ),
                            refund_error_code: Some("NOT_IMPLEMENTED".to_string()),
                            updated_by: storage_scheme.to_string(),
                            connector_refund_id: None,
                            processor_refund_data: None,
                            unified_code: None,
                            unified_message: None,
                        })
                    }
                    errors::ConnectorError::NotSupported { message, connector } => {
                        Some(storage::RefundUpdate::ErrorUpdate {
                            refund_status: Some(enums::RefundStatus::Failure),
                            refund_error_message: Some(format!(
                                "{message} is not supported by {connector}"
                            )),
                            refund_error_code: Some("NOT_SUPPORTED".to_string()),
                            updated_by: storage_scheme.to_string(),
                            connector_refund_id: None,
                            processor_refund_data: None,
                            unified_code: None,
                            unified_message: None,
                        })
                    }
                    _ => None,
                });
        // Update the refund status as failure if connector_error is NotImplemented
        if let Some(refund_error_update) = option_refund_error_update {
            state
                .store
                .update_refund(
                    refund.to_owned(),
                    refund_error_update,
                    merchant_account.storage_scheme,
                )
                .await
                .to_not_found_response(errors::ApiErrorResponse::InternalServerError)
                .attach_printable_lazy(|| {
                    format!(
                        "Failed while updating refund: refund_id: {}",
                        refund.id.get_string_repr()
                    )
                })?;
        }
        let mut refund_router_data_res = router_data_res.to_refund_failed_response()?;
        // Initiating Integrity check
        let integrity_result = check_refund_integrity(
            &refund_router_data_res.request,
            &refund_router_data_res.response,
        );
        refund_router_data_res.integrity_check = integrity_result;
        refund_router_data_res
    } else {
        router_data
    };

    let refund_update = match router_data_res.response {
        Err(err) => {
            let option_gsm = helpers::get_gsm_record(
                state,
                Some(err.code.clone()),
                Some(err.message.clone()),
                connector.connector_name.to_string(),
                consts::REFUND_FLOW_STR.to_string(),
            )
            .await;
            // Note: Some connectors do not have a separate list of refund errors
            // In such cases, the error codes and messages are stored under "Authorize" flow in GSM table
            // So we will have to fetch the GSM using Authorize flow in case GSM is not found using "refund_flow"
            let option_gsm = if option_gsm.is_none() {
                helpers::get_gsm_record(
                    state,
                    Some(err.code.clone()),
                    Some(err.message.clone()),
                    connector.connector_name.to_string(),
                    consts::AUTHORIZE_FLOW_STR.to_string(),
                )
                .await
            } else {
                option_gsm
            };

            let gsm_unified_code = option_gsm.as_ref().and_then(|gsm| gsm.unified_code.clone());
            let gsm_unified_message = option_gsm.and_then(|gsm| gsm.unified_message);

            let (unified_code, unified_message) = if let Some((code, message)) =
                gsm_unified_code.as_ref().zip(gsm_unified_message.as_ref())
            {
                (code.to_owned(), message.to_owned())
            } else {
                (
                    consts::DEFAULT_UNIFIED_ERROR_CODE.to_owned(),
                    consts::DEFAULT_UNIFIED_ERROR_MESSAGE.to_owned(),
                )
            };

            storage::RefundUpdate::ErrorUpdate {
                refund_status: Some(enums::RefundStatus::Failure),
                refund_error_message: err.reason.or(Some(err.message)),
                refund_error_code: Some(err.code),
                updated_by: storage_scheme.to_string(),
                connector_refund_id: None,
                processor_refund_data: None,
                unified_code: Some(unified_code),
                unified_message: Some(unified_message),
            }
        }
        Ok(response) => {
            // match on connector integrity checks
            match router_data_res.integrity_check.clone() {
                Err(err) => {
                    let (refund_connector_transaction_id, processor_refund_data) =
                        err.connector_transaction_id.map_or((None, None), |txn_id| {
                            let (refund_id, refund_data) =
                                ConnectorTransactionId::form_id_and_data(txn_id);
                            (Some(refund_id), refund_data)
                        });
                    metrics::INTEGRITY_CHECK_FAILED.add(
                        1,
                        router_env::metric_attributes!(
                            ("connector", connector.connector_name.to_string()),
                            ("merchant_id", merchant_account.get_id().clone()),
                        ),
                    );
                    storage::RefundUpdate::ErrorUpdate {
                        refund_status: Some(enums::RefundStatus::ManualReview),
                        refund_error_message: Some(format!(
                            "Integrity Check Failed! as data mismatched for fields {}",
                            err.field_names
                        )),
                        refund_error_code: Some("IE".to_string()),
                        updated_by: storage_scheme.to_string(),
                        connector_refund_id: refund_connector_transaction_id,
                        processor_refund_data,
                        unified_code: None,
                        unified_message: None,
                    }
                }
                Ok(()) => {
                    if response.refund_status == diesel_models::enums::RefundStatus::Success {
                        metrics::SUCCESSFUL_REFUND.add(
                            1,
                            router_env::metric_attributes!((
                                "connector",
                                connector.connector_name.to_string(),
                            )),
                        )
                    }
                    let (connector_refund_id, processor_refund_data) =
                        ConnectorTransactionId::form_id_and_data(response.connector_refund_id);
                    storage::RefundUpdate::Update {
                        connector_refund_id,
                        refund_status: response.refund_status,
                        sent_to_gateway: true,
                        refund_error_message: None,
                        refund_arn: "".to_string(),
                        updated_by: storage_scheme.to_string(),
                        processor_refund_data,
                    }
                }
            }
        }
    };

    let response = state
        .store
        .update_refund(
            refund.to_owned(),
            refund_update,
            merchant_account.storage_scheme,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::InternalServerError)
        .attach_printable_lazy(|| {
            format!(
                "Failed while updating refund: refund_id: {}",
                refund.id.get_string_repr()
            )
        })?;

    // Need to implement refunds outgoing webhooks here.
    Ok(response)
}

pub fn check_refund_integrity<T, Request>(
    request: &Request,
    refund_response_data: &Result<types::RefundsResponseData, ErrorResponse>,
) -> Result<(), common_utils::errors::IntegrityCheckError>
where
    T: FlowIntegrity,
    Request: GetIntegrityObject<T> + CheckIntegrity<Request, T>,
{
    let connector_refund_id = refund_response_data
        .as_ref()
        .map(|resp_data| resp_data.connector_refund_id.clone())
        .ok();

    request.check_integrity(request, connector_refund_id.to_owned())
}

// ********************************************** VALIDATIONS **********************************************

#[instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn validate_and_create_refund(
    state: &SessionState,
    merchant_account: &domain::MerchantAccount,
    key_store: &domain::MerchantKeyStore,
    payment_attempt: &storage::PaymentAttempt,
    payment_intent: &storage::PaymentIntent,
    refund_amount: MinorUnit,
    req: refunds::RefundsCreateRequest,
) -> RouterResult<refunds::RefundResponse> {
    let db = &*state.store;

    let refund_type = req.refund_type.unwrap_or_default();

    let merchant_reference_id = req.merchant_reference_id;

    let predicate = req
        .merchant_id
        .as_ref()
        .map(|merchant_id| merchant_id != merchant_account.get_id());

    let id = req
        .global_refund_id
        .clone()
        .ok_or(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Global refund id not found")?;

    utils::when(predicate.unwrap_or(false), || {
        Err(report!(errors::ApiErrorResponse::InvalidDataFormat {
            field_name: "merchant_id".to_string(),
            expected_format: "merchant_id from merchant account".to_string()
        })
        .attach_printable("invalid merchant_id in request"))
    })?;

    let connector_payment_id = payment_attempt.clone().connector_payment_id.ok_or_else(|| {
        report!(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Transaction in invalid. Missing field \"connector_transaction_id\" in payment_attempt.")
    })?;

    let all_refunds = db
        .find_refund_by_merchant_id_connector_transaction_id(
            merchant_account.get_id(),
            &connector_payment_id,
            merchant_account.storage_scheme,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::RefundNotFound)?;

    let currency = payment_intent.amount_details.currency;

    refunds_validator::validate_payment_order_age(
        &payment_intent.created_at,
        state.conf.refund.max_age,
    )
    .change_context(errors::ApiErrorResponse::InvalidDataFormat {
        field_name: "created_at".to_string(),
        expected_format: format!(
            "created_at not older than {} days",
            state.conf.refund.max_age,
        ),
    })?;

    let total_amount_captured = payment_intent
        .amount_captured
        .unwrap_or(payment_attempt.get_total_amount());

    refunds_validator::validate_refund_amount(
        total_amount_captured.get_amount_as_i64(),
        &all_refunds,
        refund_amount.get_amount_as_i64(),
    )
    .change_context(errors::ApiErrorResponse::RefundAmountExceedsPaymentAmount)?;

    refunds_validator::validate_maximum_refund_against_payment_attempt(
        &all_refunds,
        state.conf.refund.max_attempts,
    )
    .change_context(errors::ApiErrorResponse::MaximumRefundCount)?;

    let connector = payment_attempt
        .connector
        .clone()
        .ok_or(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("No connector populated in payment attempt")?;
    let (connector_transaction_id, processor_transaction_data) =
        ConnectorTransactionId::form_id_and_data(connector_payment_id);
    let refund_create_req = storage::RefundNew {
        id,
        merchant_reference_id: merchant_reference_id.clone(),
        external_reference_id: Some(merchant_reference_id.get_string_repr().to_string()),
        payment_id: req.payment_id,
        merchant_id: merchant_account.get_id().clone(),
        connector_transaction_id,
        connector,
        refund_type: enums::RefundType::foreign_from(req.refund_type.unwrap_or_default()),
        total_amount: payment_attempt.get_total_amount(),
        refund_amount,
        currency,
        created_at: common_utils::date_time::now(),
        modified_at: common_utils::date_time::now(),
        refund_status: enums::RefundStatus::Pending,
        metadata: req.metadata,
        description: req.reason.clone(),
        attempt_id: payment_attempt.id.clone(),
        refund_reason: req.reason,
        profile_id: Some(payment_intent.profile_id.clone()),
        connector_id: payment_attempt.merchant_connector_id.clone(),
        charges: None,
        split_refunds: None,
        connector_refund_id: None,
        sent_to_gateway: Default::default(),
        refund_arn: None,
        updated_by: Default::default(),
        organization_id: merchant_account.organization_id.clone(),
        processor_transaction_data,
        processor_refund_data: None,
    };

    let refund = match db
        .insert_refund(refund_create_req, merchant_account.storage_scheme)
        .await
    {
        Ok(refund) => {
            Box::pin(schedule_refund_execution(
                state,
                refund.clone(),
                refund_type,
                merchant_account,
                key_store,
                payment_attempt,
                payment_intent,
            ))
            .await?
        }
        Err(err) => {
            if err.current_context().is_db_unique_violation() {
                Err(errors::ApiErrorResponse::DuplicateRefundRequest)?
            } else {
                return Err(err)
                    .change_context(errors::ApiErrorResponse::RefundNotFound)
                    .attach_printable("Inserting Refund failed");
            }
        }
    };
    let unified_translated_message = if let (Some(unified_code), Some(unified_message)) =
        (refund.unified_code.clone(), refund.unified_message.clone())
    {
        helpers::get_unified_translation(
            state,
            unified_code,
            unified_message.clone(),
            state.locale.to_string(),
        )
        .await
        .or(Some(unified_message))
    } else {
        refund.unified_message
    };

    let refund = storage::Refund {
        unified_message: unified_translated_message,
        ..refund
    };

    api::RefundResponse::foreign_try_from(refund)
}

impl ForeignTryFrom<storage::Refund> for api::RefundResponse {
    type Error = error_stack::Report<errors::ApiErrorResponse>;
    fn foreign_try_from(refund: storage::Refund) -> Result<Self, Self::Error> {
        let refund = refund;

        let profile_id = refund
            .profile_id
            .clone()
            .ok_or(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Profile id not found")?;

        let merchant_connector_id = refund
            .connector_id
            .clone()
            .ok_or(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Connector id not found")?;

        Ok(Self {
            payment_id: refund.payment_id,
            id: refund.id.clone(),
            amount: refund.refund_amount,
            currency: refund.currency,
            reason: refund.refund_reason,
            status: refunds::RefundStatus::foreign_from(refund.refund_status),
            profile_id,
            metadata: refund.metadata,
            created_at: refund.created_at,
            updated_at: refund.modified_at,
            connector: refund.connector,
            merchant_connector_id,
            merchant_reference_id: Some(refund.merchant_reference_id),
            error_details: Some(RefundErrorDetails {
                code: refund.refund_error_code.unwrap_or_default(),
                message: refund.refund_error_message.unwrap_or_default(),
            }),
            connector_refund_reference_id: Some(refund.id.get_string_repr().to_string()),
        })
    }
}

// ********************************************** PROCESS TRACKER **********************************************

#[instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn schedule_refund_execution(
    state: &SessionState,
    refund: storage::Refund,
    refund_type: api_models::refunds::RefundType,
    merchant_account: &domain::MerchantAccount,
    key_store: &domain::MerchantKeyStore,
    payment_attempt: &storage::PaymentAttempt,
    payment_intent: &storage::PaymentIntent,
) -> RouterResult<storage::Refund> {
    let db = &*state.store;
    let runner = storage::ProcessTrackerRunner::RefundWorkflowRouter;
    let task = "EXECUTE_REFUND";
    let task_id = format!("{runner}_{task}_{}", refund.id.get_string_repr());

    let refund_process = db
        .find_process_by_id(&task_id)
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to find the process id")?;

    let result = match refund.refund_status {
        enums::RefundStatus::Pending | enums::RefundStatus::ManualReview => {
            match (refund.sent_to_gateway, refund_process) {
                (false, None) => {
                    // Execute the refund task based on refund_type
                    match refund_type {
                        api_models::refunds::RefundType::Scheduled => {
                            add_refund_execute_task(db, &refund, runner)
                                .await
                                .change_context(errors::ApiErrorResponse::InternalServerError)
                                .attach_printable_lazy(|| format!("Failed while pushing refund execute task to scheduler, refund_id: {}", refund.id.get_string_repr()))?;

                            Ok(refund)
                        }
                        api_models::refunds::RefundType::Instant => {
                            let update_refund = Box::pin(trigger_refund_to_gateway(
                                state,
                                &refund,
                                merchant_account,
                                key_store,
                                payment_attempt,
                                payment_intent,
                            ))
                            .await;

                            match update_refund {
                                Ok(updated_refund_data) => {
                                    add_refund_sync_task(db, &updated_refund_data, runner)
                                        .await
                                        .change_context(errors::ApiErrorResponse::InternalServerError)
                                        .attach_printable_lazy(|| format!(
                                            "Failed while pushing refund sync task in scheduler: refund_id: {}",
                                            refund.id.get_string_repr()
                                        ))?;
                                    Ok(updated_refund_data)
                                }
                                Err(err) => Err(err),
                            }
                        }
                    }
                }
                _ => {
                    // Sync the refund for status check
                    //[#300]: return refund status response
                    match refund_type {
                        api_models::refunds::RefundType::Scheduled => {
                            add_refund_sync_task(db, &refund, runner)
                                .await
                                .change_context(errors::ApiErrorResponse::InternalServerError)
                                .attach_printable_lazy(|| format!("Failed while pushing refund sync task in scheduler: refund_id: {}", refund.id.get_string_repr()))?;
                            Ok(refund)
                        }
                        api_models::refunds::RefundType::Instant => {
                            // [#255]: This is not possible in schedule_refund_execution as it will always be scheduled
                            // sync_refund_with_gateway(data, &refund).await
                            Ok(refund)
                        }
                    }
                }
            }
        }
        //  [#255]: This is not allowed to be otherwise or all
        _ => Ok(refund),
    }?;
    Ok(result)
}

#[instrument]
pub fn refund_to_refund_core_workflow_model(
    refund: &storage::Refund,
) -> storage::RefundCoreWorkflow {
    storage::RefundCoreWorkflow {
        refund_id: refund.id.clone(),
        connector_transaction_id: refund.connector_transaction_id.clone(),
        merchant_id: refund.merchant_id.clone(),
        payment_id: refund.payment_id.clone(),
        processor_transaction_data: refund.processor_transaction_data.clone(),
    }
}

#[instrument(skip_all)]
pub async fn add_refund_execute_task(
    db: &dyn db::StorageInterface,
    refund: &storage::Refund,
    runner: storage::ProcessTrackerRunner,
) -> RouterResult<storage::ProcessTracker> {
    let task = "EXECUTE_REFUND";
    let process_tracker_id = format!("{runner}_{task}_{}", refund.id.get_string_repr());
    let tag = ["REFUND"];
    let schedule_time = common_utils::date_time::now();
    let refund_workflow_tracking_data = refund_to_refund_core_workflow_model(refund);
    let process_tracker_entry = storage::ProcessTrackerNew::new(
        process_tracker_id,
        task,
        runner,
        tag,
        refund_workflow_tracking_data,
        schedule_time,
        hyperswitch_domain_models::consts::API_VERSION,
    )
    .change_context(errors::ApiErrorResponse::InternalServerError)
    .attach_printable("Failed to construct refund execute process tracker task")?;

    let response = db
        .insert_process(process_tracker_entry)
        .await
        .to_duplicate_response(errors::ApiErrorResponse::DuplicateRefundRequest)
        .attach_printable_lazy(|| {
            format!(
                "Failed while inserting task in process_tracker: refund_id: {}",
                refund.id.get_string_repr()
            )
        })?;
    Ok(response)
}

#[instrument(skip_all)]
pub async fn add_refund_sync_task(
    db: &dyn db::StorageInterface,
    refund: &storage::Refund,
    runner: storage::ProcessTrackerRunner,
) -> RouterResult<storage::ProcessTracker> {
    let task = "SYNC_REFUND";
    let process_tracker_id = format!("{runner}_{task}_{}", refund.id.get_string_repr());
    let schedule_time = common_utils::date_time::now();
    let refund_workflow_tracking_data = refund_to_refund_core_workflow_model(refund);
    let tag = ["REFUND"];
    let process_tracker_entry = storage::ProcessTrackerNew::new(
        process_tracker_id,
        task,
        runner,
        tag,
        refund_workflow_tracking_data,
        schedule_time,
        hyperswitch_domain_models::consts::API_VERSION,
    )
    .change_context(errors::ApiErrorResponse::InternalServerError)
    .attach_printable("Failed to construct refund sync process tracker task")?;

    let response = db
        .insert_process(process_tracker_entry)
        .await
        .to_duplicate_response(errors::ApiErrorResponse::DuplicateRefundRequest)
        .attach_printable_lazy(|| {
            format!(
                "Failed while inserting task in process_tracker: refund_id: {}",
                refund.id.get_string_repr()
            )
        })?;
    metrics::TASKS_ADDED_COUNT.add(1, router_env::metric_attributes!(("flow", "Refund")));

    Ok(response)
}
