use common_enums::{self, IntentStatus};
use common_utils::{self, ext_traits::OptionExt, id_type, types::keymanager::KeyManagerState};
use diesel_models::{enums, process_tracker::business_status};
use error_stack::{self, ResultExt};
use hyperswitch_domain_models::{
    business_profile, merchant_account,
    merchant_key_store::MerchantKeyStore,
    payments::{payment_attempt::PaymentAttempt, PaymentConfirmData, PaymentIntent},
};

use crate::{
    core::{
        errors::{self, RouterResult},
        passive_churn_recovery::{self as core_pcr},
    },
    db::StorageInterface,
    logger,
    routes::SessionState,
    types::{
        api::payments as api_types,
        storage::{self, passive_churn_recovery as pcr_storage_types},
        transformers::ForeignInto,
    },
    workflows::passive_churn_recovery_workflow::get_schedule_time_to_retry_mit_payments,
};

type RecoveryResult<T> = error_stack::Result<T, errors::RecoveryError>;

/// The status of Passive Churn Payments
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub enum PCRAttemptStatus {
    Succeeded,
    Failed,
    Processing,
    InvalidAction(String),
    //  Cancelled,
}

impl PCRAttemptStatus {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn update_pt_status_based_on_attempt_status(
        &self,
        db: &dyn StorageInterface,
        merchant_id: &id_type::MerchantId,
        pt_psync_process: storage::ProcessTracker,
        process: &storage::ProcessTracker,
        _key_manager_state: &KeyManagerState,
        _payment_intent: PaymentIntent,
        _merchant_key_store: &MerchantKeyStore,
        _storage_scheme: common_enums::MerchantStorageScheme,
    ) -> Result<(), errors::ProcessTrackerError> {
        match &self {
            Self::Succeeded => {
                // finish psync task as the payment was a success
                db.as_scheduler()
                    .finish_process_with_business_status(
                        pt_psync_process,
                        business_status::PSYNC_WORKFLOW_COMPLETE,
                    )
                    .await?;
                // TODO: send back the successful webhook

                // finish the current execute task as the payment has been completed
                db.as_scheduler()
                    .finish_process_with_business_status(
                        process.clone(),
                        business_status::EXECUTE_WORKFLOW_COMPLETE,
                    )
                    .await?;
            }

            Self::Failed => {
                // finish psync task
                db.as_scheduler()
                    .finish_process_with_business_status(
                        pt_psync_process.clone(),
                        business_status::PSYNC_WORKFLOW_COMPLETE,
                    )
                    .await?;

                // get a reschedule time
                let schedule_time = get_schedule_time_to_retry_mit_payments(
                    db,
                    merchant_id,
                    process.retry_count + 1,
                )
                .await;

                // check if retry is possible
                if let Some(schedule_time) = schedule_time {
                    // schedule a retry
                    db.retry_process(process.clone(), schedule_time).await?;
                } else {
                    // TODO: Record a failure back to the billing connector
                }
            }

            Self::Processing => {
                // finish the current execute task
                db.as_scheduler()
                    .finish_process_with_business_status(
                        process.clone(),
                        business_status::EXECUTE_WORKFLOW_COMPLETE_FOR_PSYNC,
                    )
                    .await?;
            }

            Self::InvalidAction(action) => {
                logger::debug!(
                    "Invalid Attempt Status for the Recovery Payment : {}",
                    action
                );
                let pt_update = storage::ProcessTrackerUpdate::StatusUpdate {
                    status: enums::ProcessTrackerStatus::Review,
                    business_status: Some(String::from(
                        business_status::EXECUTE_WORKFLOW_COMPLETE_FOR_PSYNC,
                    )),
                };
                // update the process tracker status as Review
                db.as_scheduler()
                    .update_process(process.clone(), pt_update)
                    .await?;
            }
        };
        Ok(())
    }

    pub(crate) async fn update_pt_status_based_on_attempt_status_for_psync_task(
        &self,
        state: &SessionState,
        process_tracker: storage::ProcessTracker,
        pcr_data: &pcr_storage_types::PCRPaymentData,
        key_manager_state: &KeyManagerState,
        tracking_data: &pcr_storage_types::PCRWorkflowTrackingData,
        payment_intent: &PaymentIntent,
    ) -> Result<(), errors::ProcessTrackerError> {
        let db = &*state.store;

        match self {
            Self::Succeeded => {
                // finish psync task as the payment was a success
                db.as_scheduler()
                    .finish_process_with_business_status(
                        process_tracker,
                        business_status::PSYNC_WORKFLOW_COMPLETE,
                    )
                    .await?;
                // TODO: send back the successful webhook
            }
            Self::Failed => {
                // finish psync task
                db.as_scheduler()
                    .finish_process_with_business_status(
                        process_tracker.clone(),
                        business_status::PSYNC_WORKFLOW_COMPLETE,
                    )
                    .await?;

                // get a reschedule time
                let schedule_time = get_schedule_time_to_retry_mit_payments(
                    db,
                    &pcr_data.merchant_account.get_id().clone(),
                    process_tracker.retry_count + 1,
                )
                .await;

                // check if retry is possible
                if let Some(schedule_time) = schedule_time {
                    // schedule a retry
                    db.as_scheduler()
                        .retry_process(process_tracker.clone(), schedule_time)
                        .await?;
                } else {
                    // TODO: Record a failure back to the billing connector
                }

                // TODO: Update connecter called field and active attempt
            }
            Self::Processing => {
                // do a psync payment
                let action = Box::pin(Action::payment_sync_call(
                    state,
                    pcr_data,
                    tracking_data,
                    &process_tracker,
                ))
                .await?;

                //handle the resp
                action
                    .psync_response_handler(
                        db,
                        &process_tracker,
                        tracking_data,
                        pcr_data,
                        key_manager_state,
                        payment_intent,
                    )
                    .await?;
            }
            Self::InvalidAction(action) => logger::debug!(
                "Invalid Attempt Status for the Recovery Payment : {}",
                action
            ),
        }
        Ok(())
    }
}
pub enum Decision {
    ExecuteTask,
    PsyncTask(PaymentAttempt),
    InvalidTask,
    ReviewTaskSuccessfulPayment,
    ReviewTaskFailedPayment,
}

impl Decision {
    pub async fn get_decision_based_on_params(
        state: &SessionState,
        intent_status: IntentStatus,
        called_connector: bool,
        active_attempt_id: Option<id_type::GlobalAttemptId>,
        key_manager_state: &KeyManagerState,
        merchant_key_store: &MerchantKeyStore,
        merchant_account: &merchant_account::MerchantAccount,
        payment_id: &id_type::GlobalPaymentId,
    ) -> RecoveryResult<Self> {
        Ok(match (intent_status, called_connector, active_attempt_id) {
            (IntentStatus::Failed, false, None) => Self::ExecuteTask,
            (IntentStatus::Processing, true, Some(_)) => {
                let psync_data = core_pcr::call_psync_api(state, payment_id, pcr_data).await?;
                let payment_attempt = psync_data
                    .payment_attempt
                    .get_required_value("Payment Attempt")?;
                Self::PsyncTask(payment_attempt)
            }
            (IntentStatus::Failed, true, Some(_)) => Self::ReviewTaskFailedPayment,
            (IntentStatus::Succeeded, true, Some(_)) => Self::ReviewTaskSuccessfulPayment,
            _ => Self::InvalidTask,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    SyncPayment,
    RetryPayment,
    TerminalFailure,
    SuccessfulPayment,
    ReviewPayment,
    ManualReviewAction,
}
impl Action {
    pub async fn execute_payment(
        db: &dyn StorageInterface,
        merchant_id: &id_type::MerchantId,
        payment_intent: &PaymentIntent,
        execute_task_process: &storage::ProcessTracker,
    ) -> RecoveryResult<Self> {
        // call the proxy api
        let response = core_pcr::call_proxy_api::<api_types::Authorize>(payment_intent);
        // handle proxy api's response
        match response {
            Ok(payment_data) => match payment_data.payment_attempt.status.foreign_into() {
                PCRAttemptStatus::Succeeded => Ok(Self::SuccessfulPayment),
                PCRAttemptStatus::Failed => {
                    Self::decide_retry_failure_action(db, merchant_id, execute_task_process.clone())
                        .await
                }

                PCRAttemptStatus::Processing => Ok(Self::SyncPayment),
                PCRAttemptStatus::InvalidAction(action) => {
                    logger::info!(?action, "Invalid Payment Status For PCR Payment");
                    Ok(Self::ManualReviewAction)
                }
            },
            Err(_) =>
            // check for an active attempt being constructed or not
            {
                match payment_intent.active_attempt_id.clone() {
                    Some(_) => Ok(Self::SyncPayment),
                    None => Ok(Self::ReviewPayment),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute_payment_task_response_handler(
        &self,
        db: &dyn StorageInterface,
        merchant_account: &merchant_account::MerchantAccount,
        payment_intent: &PaymentIntent,
        key_manager_state: &KeyManagerState,
        merchant_key_store: &MerchantKeyStore,
        execute_task_process: &storage::ProcessTracker,
        profile: &business_profile::Profile,
    ) -> Result<(), errors::ProcessTrackerError> {
        match self {
            Self::SyncPayment => {
                core_pcr::insert_psync_pcr_task(
                    db,
                    merchant_account.get_id().to_owned(),
                    payment_intent.id.clone(),
                    profile.get_id().to_owned(),
                    payment_intent.active_attempt_id.clone(),
                    storage::ProcessTrackerRunner::PassiveRecoveryWorkflow,
                )
                .await
                .change_context(errors::RecoveryError::ProcessTrackerFailure)
                .attach_printable("Failed to create a psync workflow in the process tracker")?;

                db.as_scheduler()
                    .finish_process_with_business_status(
                        execute_task_process.clone(),
                        business_status::EXECUTE_WORKFLOW_COMPLETE_FOR_PSYNC,
                    )
                    .await
                    .change_context(errors::RecoveryError::ProcessTrackerFailure)
                    .attach_printable("Failed to update the process tracker")?;
                Ok(())
            }

            Self::RetryPayment => {
                let mut pt = execute_task_process.clone();
                // update the schedule time
                pt.schedule_time = get_schedule_time_to_retry_mit_payments(
                    db,
                    merchant_account.get_id(),
                    pt.retry_count + 1,
                )
                .await;

                let pt_task_update = diesel_models::ProcessTrackerUpdate::StatusUpdate {
                    status: storage::enums::ProcessTrackerStatus::Pending,
                    business_status: Some(business_status::PENDING.to_owned()),
                };
                db.as_scheduler()
                    .update_process(pt.clone(), pt_task_update)
                    .await?;
                // TODO: update the connector called field and make the active attempt None

                Ok(())
            }
            Self::TerminalFailure => {
                // TODO: Record a failure transaction back to Billing Connector
                Ok(())
            }
            Self::SuccessfulPayment => Ok(()),
            Self::ReviewPayment => Ok(()),
            Self::ManualReviewAction => {
                logger::debug!("Invalid Payment Status For PCR Payment");
                let pt_update = storage::ProcessTrackerUpdate::StatusUpdate {
                    status: enums::ProcessTrackerStatus::Review,
                    business_status: Some(String::from(
                        business_status::EXECUTE_WORKFLOW_COMPLETE_FOR_PSYNC,
                    )),
                };
                // update the process tracker status as Review
                db.as_scheduler()
                    .update_process(execute_task_process.clone(), pt_update)
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn payment_sync_call(
        state: &SessionState,
        pcr_data: &pcr_storage_types::PCRPaymentData,
        tracking_data: &pcr_storage_types::PCRWorkflowTrackingData,
        process: &storage::ProcessTracker,
    ) -> RecoveryResult<Self> {
        let response =
            core_pcr::call_psync_api(state, &tracking_data.global_payment_id, pcr_data).await;
        let db = &*state.store;
        let active_attempt_id = tracking_data.payment_attempt_id.clone();
        match response {
            Ok(payment_data) => {
                // if a sync task
                let payment_attempt = payment_data
                    .payment_attempt
                    .get_required_value("Payment Attempt")?;
                match payment_attempt.status.foreign_into() {
                    PCRAttemptStatus::Succeeded => Ok(Self::SuccessfulPayment),
                    PCRAttemptStatus::Failed => {
                        Self::decide_retry_failure_action(
                            db,
                            &tracking_data.merchant_id,
                            process.clone(),
                        )
                        .await
                    }

                    PCRAttemptStatus::Processing => Ok(Self::SyncPayment),
                    PCRAttemptStatus::InvalidAction(action) => {
                        logger::info!(?action, "Invalid Payment Status For PCR PSync Payment");
                        Ok(Self::ManualReviewAction)
                    }
                }
            }
            Err(_) =>
            // check for an active attempt being present or not
            {
                match active_attempt_id.clone() {
                    Some(_) => Ok(Self::SyncPayment),
                    None => Ok(Self::ReviewPayment),
                }
            }
        }
    }
    pub async fn psync_response_handler(
        &self,
        db: &dyn StorageInterface,
        psync_task_process: &storage::ProcessTracker,
        tracking_data: &pcr_storage_types::PCRWorkflowTrackingData,
        pcr_data: &pcr_storage_types::PCRPaymentData,
        key_manager_state: &KeyManagerState,
        payment_intent: &PaymentIntent,
    ) -> RecoveryResult<()> {
        match self {
            Self::SyncPayment => {
                // retry the Psync Taks
                let pt_task_update = diesel_models::ProcessTrackerUpdate::StatusUpdate {
                    status: storage::enums::ProcessTrackerStatus::Pending,
                    business_status: Some(business_status::PENDING.to_owned()),
                };
                db.as_scheduler()
                    .update_process(psync_task_process.clone(), pt_task_update)
                    .await
                    .change_context(errors::RecoveryError::ProcessTrackerFailure)
                    .attach_printable("Failed to update the process tracker")?;
                Ok(())
            }

            Self::RetryPayment => {
                // finish the psync task
                db.as_scheduler()
                    .finish_process_with_business_status(
                        psync_task_process.clone(),
                        business_status::PSYNC_WORKFLOW_COMPLETE,
                    )
                    .await
                    .change_context(errors::RecoveryError::ProcessTrackerFailure)
                    .attach_printable("Failed to update the process tracker")?;

                // TODO: Update connecter called field and active attempt

                // retry the execute task
                let task = "EXECUTE_WORKFLOW";
                let runner = storage::ProcessTrackerRunner::PassiveRecoveryWorkflow;
                let process_tracker_id = format!(
                    "{runner}_{task}_{}",
                    tracking_data.global_payment_id.get_string_repr()
                );
                let execute_task_process = db
                    .as_scheduler()
                    .find_process_by_id(&process_tracker_id)
                    .await
                    .change_context(errors::RecoveryError::ProcessTrackerFailure)?
                    .ok_or(errors::RecoveryError::ProcessTrackerFailure)?;

                let pt_task_update = diesel_models::ProcessTrackerUpdate::StatusUpdate {
                    status: storage::enums::ProcessTrackerStatus::Pending,
                    business_status: Some(business_status::PENDING.to_owned()),
                };

                db.as_scheduler()
                    .update_process(execute_task_process, pt_task_update)
                    .await
                    .change_context(errors::RecoveryError::ProcessTrackerFailure)
                    .attach_printable("Failed to update the process tracker")?;
                Ok(())
            }
            Self::TerminalFailure => {
                // TODO: Record a failure transaction back to Billing Connector
                // finish the current psync task
                db.as_scheduler()
                    .finish_process_with_business_status(
                        psync_task_process.clone(),
                        business_status::PSYNC_WORKFLOW_COMPLETE,
                    )
                    .await
                    .change_context(errors::RecoveryError::ProcessTrackerFailure)
                    .attach_printable("Failed to update the process tracker")?;
                Ok(())
            }
            Self::SuccessfulPayment => {
                // TODO: Record a successful transaction back to Billing Connector
                // finish the current psync task
                db.as_scheduler()
                    .finish_process_with_business_status(
                        psync_task_process.clone(),
                        business_status::PSYNC_WORKFLOW_COMPLETE,
                    )
                    .await
                    .change_context(errors::RecoveryError::ProcessTrackerFailure)
                    .attach_printable("Failed to update the process tracker")?;
                Ok(())
            }
            Self::ReviewPayment => {
                core_pcr::insert_review_task(
                    db,
                    tracking_data.clone(),
                    storage::ProcessTrackerRunner::PassiveRecoveryWorkflow,
                )
                .await?;
                db.finish_process_with_business_status(
                    execute_task_process.clone(),
                    business_status::PSYNC_WORKFLOW_COMPLETE_FOR_REVIEW,
                )
                .await?;
                Ok(())
            }
            Self::ManualReviewAction => {
                logger::debug!("Invalid Payment Status For PCR Payment");
                let pt_update = storage::ProcessTrackerUpdate::StatusUpdate {
                    status: enums::ProcessTrackerStatus::Review,
                    business_status: Some(String::from(business_status::PSYNC_WORKFLOW_COMPLETE)),
                };
                // update the process tracker status as Review
                db.as_scheduler()
                    .update_process(psync_task_process.clone(), pt_update)
                    .await
                    .change_context(errors::RecoveryError::ProcessTrackerFailure)
                    .attach_printable("Failed to update the process tracker")?;

                Ok(())
            }
        }
    }

    pub(crate) async fn decide_retry_failure_action(
        db: &dyn StorageInterface,
        merchant_id: &id_type::MerchantId,
        pt: storage::ProcessTracker,
    ) -> RecoveryResult<Self> {
        let schedule_time =
            get_schedule_time_to_retry_mit_payments(db, merchant_id, pt.retry_count + 1).await;
        match schedule_time {
            Some(_) => Ok(Self::RetryPayment),

            None => Ok(Self::TerminalFailure),
        }
    }
}
