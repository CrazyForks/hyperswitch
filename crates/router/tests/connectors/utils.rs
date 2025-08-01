use std::{fmt::Debug, marker::PhantomData, str::FromStr, sync::Arc, time::Duration};

use async_trait::async_trait;
use common_utils::{id_type::GenerateId, pii::Email};
use error_stack::Report;
use masking::Secret;
use router::{
    configs::settings::Settings,
    core::{errors::ConnectorError, payments},
    db::StorageImpl,
    routes,
    services::{
        self,
        connector_integration_interface::{BoxedConnectorIntegrationInterface, ConnectorEnum},
    },
    types::{self, storage::enums, AccessToken, MinorUnit, PaymentAddress, RouterData},
};
use test_utils::connector_auth::ConnectorAuthType;
use tokio::sync::oneshot;
use wiremock::{Mock, MockServer};

pub trait Connector {
    fn get_data(&self) -> types::api::ConnectorData;

    fn get_auth_token(&self) -> types::ConnectorAuthType;

    fn get_name(&self) -> String;

    fn get_connector_meta(&self) -> Option<serde_json::Value> {
        None
    }

    /// interval in seconds to be followed when making the subsequent request whenever needed
    fn get_request_interval(&self) -> u64 {
        5
    }

    #[cfg(feature = "payouts")]
    fn get_payout_data(&self) -> Option<types::api::ConnectorData> {
        None
    }
}

pub fn construct_connector_data_old(
    connector: types::api::BoxedConnector,
    connector_name: types::Connector,
    get_token: types::api::GetToken,
    merchant_connector_id: Option<common_utils::id_type::MerchantConnectorAccountId>,
) -> types::api::ConnectorData {
    types::api::ConnectorData {
        connector: ConnectorEnum::Old(connector),
        connector_name,
        get_token,
        merchant_connector_id,
    }
}

#[derive(Debug, Default, Clone)]
pub struct PaymentInfo {
    pub address: Option<PaymentAddress>,
    pub auth_type: Option<enums::AuthenticationType>,
    pub access_token: Option<AccessToken>,
    pub connector_meta_data: Option<serde_json::Value>,
    pub connector_customer: Option<String>,
    pub payment_method_token: Option<String>,
    #[cfg(feature = "payouts")]
    pub payout_method_data: Option<types::api::PayoutMethodData>,
    #[cfg(feature = "payouts")]
    pub currency: Option<enums::Currency>,
}

impl PaymentInfo {
    pub fn with_default_billing_name() -> Self {
        Self {
            address: Some(PaymentAddress::new(
                None,
                None,
                Some(hyperswitch_domain_models::address::Address {
                    address: Some(hyperswitch_domain_models::address::AddressDetails {
                        first_name: Some(Secret::new("John".to_string())),
                        last_name: Some(Secret::new("Doe".to_string())),
                        ..Default::default()
                    }),
                    phone: None,
                    email: None,
                }),
                None,
            )),
            ..Default::default()
        }
    }
}

#[async_trait]
pub trait ConnectorActions: Connector {
    /// For initiating payments when `CaptureMethod` is set to `Manual`
    /// This doesn't complete the transaction, `PaymentsCapture` needs to be done manually
    async fn authorize_payment(
        &self,
        payment_data: Option<types::PaymentsAuthorizeData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PaymentsAuthorizeRouterData, Report<ConnectorError>> {
        let integration = self.get_data().connector.get_connector_integration();
        let request = self.generate_data(
            types::PaymentsAuthorizeData {
                confirm: true,
                capture_method: Some(diesel_models::enums::CaptureMethod::Manual),
                ..(payment_data.unwrap_or(PaymentAuthorizeType::default().0))
            },
            payment_info,
        );
        Box::pin(call_connector(request, integration)).await
    }

    async fn create_connector_customer(
        &self,
        payment_data: Option<types::ConnectorCustomerData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::ConnectorCustomerRouterData, Report<ConnectorError>> {
        let integration = self.get_data().connector.get_connector_integration();
        let request = self.generate_data(
            types::ConnectorCustomerData {
                ..(payment_data.unwrap_or(CustomerType::default().0))
            },
            payment_info,
        );
        Box::pin(call_connector(request, integration)).await
    }

    async fn create_connector_pm_token(
        &self,
        payment_data: Option<types::PaymentMethodTokenizationData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::TokenizationRouterData, Report<ConnectorError>> {
        let integration = self.get_data().connector.get_connector_integration();
        let request = self.generate_data(
            types::PaymentMethodTokenizationData {
                ..(payment_data.unwrap_or(TokenType::default().0))
            },
            payment_info,
        );
        Box::pin(call_connector(request, integration)).await
    }

    /// For initiating payments when `CaptureMethod` is set to `Automatic`
    /// This does complete the transaction without user intervention to Capture the payment
    async fn make_payment(
        &self,
        payment_data: Option<types::PaymentsAuthorizeData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PaymentsAuthorizeRouterData, Report<ConnectorError>> {
        let integration = self.get_data().connector.get_connector_integration();
        let request = self.generate_data(
            types::PaymentsAuthorizeData {
                confirm: true,
                capture_method: Some(diesel_models::enums::CaptureMethod::Automatic),
                ..(payment_data.unwrap_or(PaymentAuthorizeType::default().0))
            },
            payment_info,
        );
        Box::pin(call_connector(request, integration)).await
    }

    async fn sync_payment(
        &self,
        payment_data: Option<types::PaymentsSyncData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PaymentsSyncRouterData, Report<ConnectorError>> {
        let integration = self.get_data().connector.get_connector_integration();
        let request = self.generate_data(
            payment_data.unwrap_or_else(|| PaymentSyncType::default().0),
            payment_info,
        );
        Box::pin(call_connector(request, integration)).await
    }

    /// will retry the psync till the given status matches or retry max 3 times
    async fn psync_retry_till_status_matches(
        &self,
        status: enums::AttemptStatus,
        payment_data: Option<types::PaymentsSyncData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PaymentsSyncRouterData, Report<ConnectorError>> {
        let max_tries = 3;
        for curr_try in 0..max_tries {
            let sync_res = self
                .sync_payment(payment_data.clone(), payment_info.clone())
                .await
                .unwrap();
            if (sync_res.status == status) || (curr_try == max_tries - 1) {
                return Ok(sync_res);
            }
            tokio::time::sleep(Duration::from_secs(self.get_request_interval())).await;
        }
        Err(ConnectorError::ProcessingStepFailed(None).into())
    }

    async fn capture_payment(
        &self,
        transaction_id: String,
        payment_data: Option<types::PaymentsCaptureData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PaymentsCaptureRouterData, Report<ConnectorError>> {
        let integration = self.get_data().connector.get_connector_integration();
        let request = self.generate_data(
            types::PaymentsCaptureData {
                connector_transaction_id: transaction_id,
                ..payment_data.unwrap_or(PaymentCaptureType::default().0)
            },
            payment_info,
        );
        Box::pin(call_connector(request, integration)).await
    }

    async fn authorize_and_capture_payment(
        &self,
        authorize_data: Option<types::PaymentsAuthorizeData>,
        capture_data: Option<types::PaymentsCaptureData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PaymentsCaptureRouterData, Report<ConnectorError>> {
        let authorize_response = self
            .authorize_payment(authorize_data, payment_info.clone())
            .await
            .unwrap();
        assert_eq!(authorize_response.status, enums::AttemptStatus::Authorized);
        let txn_id = get_connector_transaction_id(authorize_response.response);
        let response = self
            .capture_payment(txn_id.unwrap(), capture_data, payment_info)
            .await
            .unwrap();
        return Ok(response);
    }

    async fn void_payment(
        &self,
        transaction_id: String,
        payment_data: Option<types::PaymentsCancelData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PaymentsCancelRouterData, Report<ConnectorError>> {
        let integration = self.get_data().connector.get_connector_integration();
        let request = self.generate_data(
            types::PaymentsCancelData {
                connector_transaction_id: transaction_id,
                ..payment_data.unwrap_or(PaymentCancelType::default().0)
            },
            payment_info,
        );
        Box::pin(call_connector(request, integration)).await
    }

    async fn authorize_and_void_payment(
        &self,
        authorize_data: Option<types::PaymentsAuthorizeData>,
        void_data: Option<types::PaymentsCancelData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PaymentsCancelRouterData, Report<ConnectorError>> {
        let authorize_response = self
            .authorize_payment(authorize_data, payment_info.clone())
            .await
            .unwrap();
        assert_eq!(authorize_response.status, enums::AttemptStatus::Authorized);
        let txn_id = get_connector_transaction_id(authorize_response.response);
        tokio::time::sleep(Duration::from_secs(self.get_request_interval())).await; // to avoid 404 error
        let response = self
            .void_payment(txn_id.unwrap(), void_data, payment_info)
            .await
            .unwrap();
        return Ok(response);
    }

    async fn refund_payment(
        &self,
        transaction_id: String,
        refund_data: Option<types::RefundsData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::RefundExecuteRouterData, Report<ConnectorError>> {
        let integration = self.get_data().connector.get_connector_integration();
        let request = self.generate_data(
            types::RefundsData {
                connector_transaction_id: transaction_id,
                ..refund_data.unwrap_or(PaymentRefundType::default().0)
            },
            payment_info,
        );
        Box::pin(call_connector(request, integration)).await
    }

    async fn capture_payment_and_refund(
        &self,
        authorize_data: Option<types::PaymentsAuthorizeData>,
        capture_data: Option<types::PaymentsCaptureData>,
        refund_data: Option<types::RefundsData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::RefundExecuteRouterData, Report<ConnectorError>> {
        //make a successful payment
        let response = self
            .authorize_and_capture_payment(authorize_data, capture_data, payment_info.clone())
            .await
            .unwrap();
        let txn_id = self.get_connector_transaction_id_from_capture_data(response);

        //try refund for previous payment
        tokio::time::sleep(Duration::from_secs(self.get_request_interval())).await; // to avoid 404 error
        Ok(self
            .refund_payment(txn_id.unwrap(), refund_data, payment_info)
            .await
            .unwrap())
    }

    async fn make_payment_and_refund(
        &self,
        authorize_data: Option<types::PaymentsAuthorizeData>,
        refund_data: Option<types::RefundsData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::RefundExecuteRouterData, Report<ConnectorError>> {
        //make a successful payment
        let response = self
            .make_payment(authorize_data, payment_info.clone())
            .await
            .unwrap();

        //try refund for previous payment
        let transaction_id = get_connector_transaction_id(response.response).unwrap();
        tokio::time::sleep(Duration::from_secs(self.get_request_interval())).await; // to avoid 404 error
        Ok(self
            .refund_payment(transaction_id, refund_data, payment_info)
            .await
            .unwrap())
    }

    async fn auth_capture_and_refund(
        &self,
        authorize_data: Option<types::PaymentsAuthorizeData>,
        refund_data: Option<types::RefundsData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::RefundExecuteRouterData, Report<ConnectorError>> {
        //make a successful payment
        let response = self
            .authorize_and_capture_payment(authorize_data, None, payment_info.clone())
            .await
            .unwrap();

        //try refund for previous payment
        let transaction_id = get_connector_transaction_id(response.response).unwrap();
        tokio::time::sleep(Duration::from_secs(self.get_request_interval())).await; // to avoid 404 error
        Ok(self
            .refund_payment(transaction_id, refund_data, payment_info)
            .await
            .unwrap())
    }

    async fn make_payment_and_multiple_refund(
        &self,
        authorize_data: Option<types::PaymentsAuthorizeData>,
        refund_data: Option<types::RefundsData>,
        payment_info: Option<PaymentInfo>,
    ) {
        //make a successful payment
        let response = self
            .make_payment(authorize_data, payment_info.clone())
            .await
            .unwrap();

        //try refund for previous payment
        let transaction_id = get_connector_transaction_id(response.response).unwrap();
        for _x in 0..2 {
            tokio::time::sleep(Duration::from_secs(self.get_request_interval())).await; // to avoid 404 error
            let refund_response = self
                .refund_payment(
                    transaction_id.clone(),
                    refund_data.clone(),
                    payment_info.clone(),
                )
                .await
                .unwrap();
            assert_eq!(
                refund_response.response.unwrap().refund_status,
                enums::RefundStatus::Success,
            );
        }
    }

    async fn sync_refund(
        &self,
        refund_id: String,
        payment_data: Option<types::RefundsData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::RefundSyncRouterData, Report<ConnectorError>> {
        let integration = self.get_data().connector.get_connector_integration();
        let request = self.generate_data(
            payment_data.unwrap_or_else(|| types::RefundsData {
                payment_amount: 1000,
                minor_payment_amount: MinorUnit::new(1000),
                currency: enums::Currency::USD,
                refund_id: uuid::Uuid::new_v4().to_string(),
                connector_transaction_id: "".to_string(),
                webhook_url: None,
                refund_amount: 100,
                minor_refund_amount: MinorUnit::new(100),
                connector_metadata: None,
                refund_connector_metadata: None,
                reason: None,
                connector_refund_id: Some(refund_id),
                browser_info: None,
                split_refunds: None,
                integrity_object: None,
                refund_status: enums::RefundStatus::Pending,
                merchant_account_id: None,
                merchant_config_currency: None,
                capture_method: None,
                additional_payment_method_data: None,
            }),
            payment_info,
        );
        Box::pin(call_connector(request, integration)).await
    }

    /// will retry the rsync till the given status matches or retry max 3 times
    async fn rsync_retry_till_status_matches(
        &self,
        status: enums::RefundStatus,
        refund_id: String,
        payment_data: Option<types::RefundsData>,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::RefundSyncRouterData, Report<ConnectorError>> {
        let max_tries = 3;
        for curr_try in 0..max_tries {
            let sync_res = self
                .sync_refund(
                    refund_id.clone(),
                    payment_data.clone(),
                    payment_info.clone(),
                )
                .await
                .unwrap();
            if (sync_res.clone().response.unwrap().refund_status == status)
                || (curr_try == max_tries - 1)
            {
                return Ok(sync_res);
            }
            tokio::time::sleep(Duration::from_secs(self.get_request_interval())).await;
        }
        Err(ConnectorError::ProcessingStepFailed(None).into())
    }

    #[cfg(feature = "payouts")]
    fn get_payout_request<Flow, Res>(
        &self,
        connector_payout_id: Option<String>,
        payout_type: enums::PayoutType,
        payment_info: Option<PaymentInfo>,
    ) -> RouterData<Flow, types::PayoutsData, Res> {
        self.generate_data(
            types::PayoutsData {
                payout_id: common_utils::id_type::PayoutId::generate(),
                amount: 1,
                minor_amount: MinorUnit::new(1),
                connector_payout_id,
                destination_currency: payment_info.to_owned().map_or(enums::Currency::EUR, |pi| {
                    pi.currency.map_or(enums::Currency::EUR, |c| c)
                }),
                source_currency: payment_info.to_owned().map_or(enums::Currency::EUR, |pi| {
                    pi.currency.map_or(enums::Currency::EUR, |c| c)
                }),
                entity_type: enums::PayoutEntityType::Individual,
                payout_type: Some(payout_type),
                customer_details: Some(payments::CustomerDetails {
                    customer_id: Some(common_utils::generate_customer_id_of_default_length()),
                    name: Some(Secret::new("John Doe".to_string())),
                    email: Email::from_str("john.doe@example").ok(),
                    phone: Some(Secret::new("620874518".to_string())),
                    phone_country_code: Some("+31".to_string()),
                }),
                vendor_details: None,
                priority: None,
                connector_transfer_method_id: None,
            },
            payment_info,
        )
    }

    fn generate_data<Flow, Req: From<Req>, Res>(
        &self,
        req: Req,
        info: Option<PaymentInfo>,
    ) -> RouterData<Flow, Req, Res> {
        let merchant_id =
            common_utils::id_type::MerchantId::try_from(std::borrow::Cow::from(self.get_name()))
                .unwrap();

        RouterData {
            flow: PhantomData,
            merchant_id,
            customer_id: Some(common_utils::generate_customer_id_of_default_length()),
            connector: self.get_name(),
            tenant_id: common_utils::id_type::TenantId::try_from_string("public".to_string())
                .unwrap(),
            payment_id: uuid::Uuid::new_v4().to_string(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
            status: enums::AttemptStatus::default(),
            auth_type: info
                .clone()
                .map_or(enums::AuthenticationType::NoThreeDs, |a| {
                    a.auth_type
                        .map_or(enums::AuthenticationType::NoThreeDs, |a| a)
                }),
            payment_method: enums::PaymentMethod::Card,
            connector_auth_type: self.get_auth_token(),
            description: Some("This is a test".to_string()),
            payment_method_status: None,
            request: req,
            response: Err(types::ErrorResponse::default()),
            address: info
                .clone()
                .and_then(|a| a.address)
                .or_else(|| Some(PaymentAddress::default()))
                .unwrap(),
            connector_meta_data: info
                .clone()
                .and_then(|a| a.connector_meta_data.map(Secret::new)),
            connector_wallets_details: None,
            amount_captured: None,
            minor_amount_captured: None,
            access_token: info.clone().and_then(|a| a.access_token),
            session_token: None,
            reference_id: None,
            payment_method_token: info.clone().and_then(|a| {
                a.payment_method_token
                    .map(|token| types::PaymentMethodToken::Token(Secret::new(token)))
            }),
            connector_customer: info.clone().and_then(|a| a.connector_customer),
            recurring_mandate_payment_data: None,

            preprocessing_id: None,
            connector_request_reference_id: uuid::Uuid::new_v4().to_string(),
            #[cfg(feature = "payouts")]
            payout_method_data: info.and_then(|p| p.payout_method_data),
            #[cfg(feature = "payouts")]
            quote_id: None,
            test_mode: None,
            payment_method_balance: None,
            connector_api_version: None,
            connector_http_status_code: None,
            apple_pay_flow: None,
            external_latency: None,
            frm_metadata: None,
            refund_id: None,
            dispute_id: None,
            connector_response: None,
            integrity_check: Ok(()),
            additional_merchant_data: None,
            header_payload: None,
            connector_mandate_request_reference_id: None,
            psd2_sca_exemption_type: None,
            authentication_id: None,
            raw_connector_response: None,
            is_payment_id_from_merchant: None,
        }
    }

    fn get_connector_transaction_id_from_capture_data(
        &self,
        response: types::PaymentsCaptureRouterData,
    ) -> Option<String> {
        match response.response {
            Ok(types::PaymentsResponseData::TransactionResponse { resource_id, .. }) => {
                resource_id.get_connector_transaction_id().ok()
            }
            Ok(types::PaymentsResponseData::SessionResponse { .. }) => None,
            Ok(types::PaymentsResponseData::SessionTokenResponse { .. }) => None,
            Ok(types::PaymentsResponseData::TokenizationResponse { .. }) => None,
            Ok(types::PaymentsResponseData::TransactionUnresolvedResponse { .. }) => None,
            Ok(types::PaymentsResponseData::ConnectorCustomerResponse { .. }) => None,
            Ok(types::PaymentsResponseData::PreProcessingResponse { .. }) => None,
            Ok(types::PaymentsResponseData::ThreeDSEnrollmentResponse { .. }) => None,
            Ok(types::PaymentsResponseData::MultipleCaptureResponse { .. }) => None,
            Ok(types::PaymentsResponseData::IncrementalAuthorizationResponse { .. }) => None,
            Ok(types::PaymentsResponseData::PostProcessingResponse { .. }) => None,
            Ok(types::PaymentsResponseData::PaymentResourceUpdateResponse { .. }) => None,
            Ok(types::PaymentsResponseData::PaymentsCreateOrderResponse { .. }) => None,
            Err(_) => None,
        }
    }

    #[cfg(feature = "payouts")]
    async fn verify_payout_eligibility(
        &self,
        payout_type: enums::PayoutType,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PayoutsResponseData, Report<ConnectorError>> {
        let connector_integration: services::BoxedPayoutConnectorIntegrationInterface<
            types::api::PoEligibility,
            types::PayoutsData,
            types::PayoutsResponseData,
        > = self
            .get_payout_data()
            .ok_or(ConnectorError::FailedToObtainPreferredConnector)?
            .connector
            .get_connector_integration();
        let request = self.get_payout_request(None, payout_type, payment_info);
        let tx: oneshot::Sender<()> = oneshot::channel().0;

        let app_state = Box::pin(routes::AppState::with_storage(
            Settings::new().unwrap(),
            StorageImpl::PostgresqlTest,
            tx,
            Box::new(services::MockApiClient),
        ))
        .await;
        let state = Arc::new(app_state)
            .get_session_state(
                &common_utils::id_type::TenantId::try_from_string("public".to_string()).unwrap(),
                None,
                || {},
            )
            .unwrap();
        let res = services::api::execute_connector_processing_step(
            &state,
            connector_integration,
            &request,
            payments::CallConnectorAction::Trigger,
            None,
            None,
        )
        .await?;
        Ok(res.response.unwrap())
    }

    #[cfg(feature = "payouts")]
    async fn fulfill_payout(
        &self,
        connector_payout_id: Option<String>,
        payout_type: enums::PayoutType,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PayoutsResponseData, Report<ConnectorError>> {
        let connector_integration: services::BoxedPayoutConnectorIntegrationInterface<
            types::api::PoFulfill,
            types::PayoutsData,
            types::PayoutsResponseData,
        > = self
            .get_payout_data()
            .ok_or(ConnectorError::FailedToObtainPreferredConnector)?
            .connector
            .get_connector_integration();
        let request = self.get_payout_request(connector_payout_id, payout_type, payment_info);
        let tx: oneshot::Sender<()> = oneshot::channel().0;

        let app_state = Box::pin(routes::AppState::with_storage(
            Settings::new().unwrap(),
            StorageImpl::PostgresqlTest,
            tx,
            Box::new(services::MockApiClient),
        ))
        .await;
        let state = Arc::new(app_state)
            .get_session_state(
                &common_utils::id_type::TenantId::try_from_string("public".to_string()).unwrap(),
                None,
                || {},
            )
            .unwrap();
        let res = services::api::execute_connector_processing_step(
            &state,
            connector_integration,
            &request,
            payments::CallConnectorAction::Trigger,
            None,
            None,
        )
        .await?;
        Ok(res.response.unwrap())
    }

    #[cfg(feature = "payouts")]
    async fn create_payout(
        &self,
        connector_customer: Option<String>,
        payout_type: enums::PayoutType,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PayoutsResponseData, Report<ConnectorError>> {
        let connector_integration: services::BoxedPayoutConnectorIntegrationInterface<
            types::api::PoCreate,
            types::PayoutsData,
            types::PayoutsResponseData,
        > = self
            .get_payout_data()
            .ok_or(ConnectorError::FailedToObtainPreferredConnector)?
            .connector
            .get_connector_integration();
        let mut request = self.get_payout_request(None, payout_type, payment_info);
        request.connector_customer = connector_customer;
        let tx: oneshot::Sender<()> = oneshot::channel().0;

        let app_state = Box::pin(routes::AppState::with_storage(
            Settings::new().unwrap(),
            StorageImpl::PostgresqlTest,
            tx,
            Box::new(services::MockApiClient),
        ))
        .await;
        let state = Arc::new(app_state)
            .get_session_state(
                &common_utils::id_type::TenantId::try_from_string("public".to_string()).unwrap(),
                None,
                || {},
            )
            .unwrap();
        let res = services::api::execute_connector_processing_step(
            &state,
            connector_integration,
            &request,
            payments::CallConnectorAction::Trigger,
            None,
            None,
        )
        .await?;
        Ok(res.response.unwrap())
    }

    #[cfg(feature = "payouts")]
    async fn cancel_payout(
        &self,
        connector_payout_id: String,
        payout_type: enums::PayoutType,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PayoutsResponseData, Report<ConnectorError>> {
        let connector_integration: services::BoxedPayoutConnectorIntegrationInterface<
            types::api::PoCancel,
            types::PayoutsData,
            types::PayoutsResponseData,
        > = self
            .get_payout_data()
            .ok_or(ConnectorError::FailedToObtainPreferredConnector)?
            .connector
            .get_connector_integration();
        let request = self.get_payout_request(Some(connector_payout_id), payout_type, payment_info);
        let tx: oneshot::Sender<()> = oneshot::channel().0;

        let app_state = Box::pin(routes::AppState::with_storage(
            Settings::new().unwrap(),
            StorageImpl::PostgresqlTest,
            tx,
            Box::new(services::MockApiClient),
        ))
        .await;
        let state = Arc::new(app_state)
            .get_session_state(
                &common_utils::id_type::TenantId::try_from_string("public".to_string()).unwrap(),
                None,
                || {},
            )
            .unwrap();
        let res = services::api::execute_connector_processing_step(
            &state,
            connector_integration,
            &request,
            payments::CallConnectorAction::Trigger,
            None,
            None,
        )
        .await?;
        Ok(res.response.unwrap())
    }

    #[cfg(feature = "payouts")]
    async fn create_and_fulfill_payout(
        &self,
        connector_customer: Option<String>,
        payout_type: enums::PayoutType,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PayoutsResponseData, Report<ConnectorError>> {
        let create_res = self
            .create_payout(connector_customer, payout_type, payment_info.to_owned())
            .await?;
        assert_eq!(
            create_res.status.unwrap(),
            enums::PayoutStatus::RequiresFulfillment
        );
        let fulfill_res = self
            .fulfill_payout(
                create_res.connector_payout_id,
                payout_type,
                payment_info.to_owned(),
            )
            .await?;
        Ok(fulfill_res)
    }

    #[cfg(feature = "payouts")]
    async fn create_and_cancel_payout(
        &self,
        connector_customer: Option<String>,
        payout_type: enums::PayoutType,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PayoutsResponseData, Report<ConnectorError>> {
        let create_res = self
            .create_payout(connector_customer, payout_type, payment_info.to_owned())
            .await?;
        assert_eq!(
            create_res.status.unwrap(),
            enums::PayoutStatus::RequiresFulfillment
        );
        let cancel_res = self
            .cancel_payout(
                create_res
                    .connector_payout_id
                    .ok_or(ConnectorError::MissingRequiredField {
                        field_name: "connector_payout_id",
                    })?,
                payout_type,
                payment_info.to_owned(),
            )
            .await?;
        Ok(cancel_res)
    }

    #[cfg(feature = "payouts")]
    async fn create_payout_recipient(
        &self,
        payout_type: enums::PayoutType,
        payment_info: Option<PaymentInfo>,
    ) -> Result<types::PayoutsResponseData, Report<ConnectorError>> {
        let connector_integration: services::BoxedPayoutConnectorIntegrationInterface<
            types::api::PoRecipient,
            types::PayoutsData,
            types::PayoutsResponseData,
        > = self
            .get_payout_data()
            .ok_or(ConnectorError::FailedToObtainPreferredConnector)?
            .connector
            .get_connector_integration();
        let request = self.get_payout_request(None, payout_type, payment_info);
        let tx = oneshot::channel().0;

        let app_state = Box::pin(routes::AppState::with_storage(
            Settings::new().unwrap(),
            StorageImpl::PostgresqlTest,
            tx,
            Box::new(services::MockApiClient),
        ))
        .await;
        let state = Arc::new(app_state)
            .get_session_state(
                &common_utils::id_type::TenantId::try_from_string("public".to_string()).unwrap(),
                None,
                || {},
            )
            .unwrap();
        let res = services::api::execute_connector_processing_step(
            &state,
            connector_integration,
            &request,
            payments::CallConnectorAction::Trigger,
            None,
            None,
        )
        .await?;
        Ok(res.response.unwrap())
    }
}

async fn call_connector<
    T: Debug + Clone + 'static,
    ResourceCommonData: Debug
        + Clone
        + services::connector_integration_interface::RouterDataConversion<T, Req, Resp>
        + 'static,
    Req: Debug + Clone + 'static,
    Resp: Debug + Clone + 'static,
>(
    request: RouterData<T, Req, Resp>,
    integration: BoxedConnectorIntegrationInterface<T, ResourceCommonData, Req, Resp>,
) -> Result<RouterData<T, Req, Resp>, Report<ConnectorError>> {
    let conf = Settings::new().unwrap();
    let tx: oneshot::Sender<()> = oneshot::channel().0;

    let app_state = Box::pin(routes::AppState::with_storage(
        conf,
        StorageImpl::PostgresqlTest,
        tx,
        Box::new(services::MockApiClient),
    ))
    .await;
    let state = Arc::new(app_state)
        .get_session_state(
            &common_utils::id_type::TenantId::try_from_string("public".to_string()).unwrap(),
            None,
            || {},
        )
        .unwrap();
    services::api::execute_connector_processing_step(
        &state,
        integration,
        &request,
        payments::CallConnectorAction::Trigger,
        None,
        None,
    )
    .await
}

pub struct MockConfig {
    pub address: Option<String>,
    pub mocks: Vec<Mock>,
}

#[async_trait]
pub trait LocalMock {
    async fn start_server(&self, config: MockConfig) -> MockServer {
        let address = config
            .address
            .unwrap_or_else(|| "127.0.0.1:9090".to_string());
        let listener = std::net::TcpListener::bind(address).unwrap();
        let expected_server_address = listener
            .local_addr()
            .expect("Failed to get server address.");
        let mock_server = MockServer::builder().listener(listener).start().await;
        assert_eq!(&expected_server_address, mock_server.address());
        for mock in config.mocks {
            mock_server.register(mock).await;
        }
        mock_server
    }
}

pub struct PaymentAuthorizeType(pub types::PaymentsAuthorizeData);
pub struct PaymentCaptureType(pub types::PaymentsCaptureData);
pub struct PaymentCancelType(pub types::PaymentsCancelData);
pub struct PaymentSyncType(pub types::PaymentsSyncData);
pub struct PaymentRefundType(pub types::RefundsData);
pub struct CCardType(pub types::domain::Card);
pub struct BrowserInfoType(pub types::BrowserInformation);
pub struct CustomerType(pub types::ConnectorCustomerData);
pub struct TokenType(pub types::PaymentMethodTokenizationData);

impl Default for CCardType {
    fn default() -> Self {
        Self(types::domain::Card {
            card_number: cards::CardNumber::from_str("4200000000000000").unwrap(),
            card_exp_month: Secret::new("10".to_string()),
            card_exp_year: Secret::new("2025".to_string()),
            card_cvc: Secret::new("999".to_string()),
            card_issuer: None,
            card_network: None,
            card_type: None,
            card_issuing_country: None,
            bank_code: None,
            nick_name: Some(Secret::new("nick_name".into())),
            card_holder_name: Some(Secret::new("card holder name".into())),
            co_badged_card_data: None,
        })
    }
}

impl Default for PaymentAuthorizeType {
    fn default() -> Self {
        let data = types::PaymentsAuthorizeData {
            payment_method_data: types::domain::PaymentMethodData::Card(CCardType::default().0),
            amount: 100,
            minor_amount: MinorUnit::new(100),
            order_tax_amount: Some(MinorUnit::zero()),
            currency: enums::Currency::USD,
            confirm: true,
            statement_descriptor_suffix: None,
            statement_descriptor: None,
            capture_method: None,
            setup_future_usage: None,
            mandate_id: None,
            off_session: None,
            setup_mandate_details: None,
            browser_info: Some(BrowserInfoType::default().0),
            order_details: None,
            order_category: None,
            email: None,
            customer_name: None,
            session_token: None,
            enrolled_for_3ds: false,
            related_transaction_id: None,
            payment_experience: None,
            payment_method_type: None,
            router_return_url: None,
            complete_authorize_url: None,
            webhook_url: None,
            customer_id: None,
            surcharge_details: None,
            request_incremental_authorization: false,
            request_extended_authorization: None,
            metadata: None,
            authentication_data: None,
            customer_acceptance: None,
            split_payments: None,
            integrity_object: None,
            merchant_order_reference_id: None,
            additional_payment_method_data: None,
            shipping_cost: None,
            merchant_account_id: None,
            merchant_config_currency: None,
            connector_testing_data: None,
            order_id: None,
            locale: None,
            payment_channel: None,
        };
        Self(data)
    }
}

impl Default for PaymentCaptureType {
    fn default() -> Self {
        Self(types::PaymentsCaptureData {
            amount_to_capture: 100,
            currency: enums::Currency::USD,
            connector_transaction_id: "".to_string(),
            payment_amount: 100,
            ..Default::default()
        })
    }
}

impl Default for PaymentCancelType {
    fn default() -> Self {
        Self(types::PaymentsCancelData {
            cancellation_reason: Some("requested_by_customer".to_string()),
            connector_transaction_id: "".to_string(),
            ..Default::default()
        })
    }
}

impl Default for BrowserInfoType {
    fn default() -> Self {
        let data = types::BrowserInformation {
            user_agent: Some("".to_string()),
            accept_header: Some("".to_string()),
            language: Some("nl-NL".to_string()),
            color_depth: Some(24),
            screen_height: Some(723),
            screen_width: Some(1536),
            time_zone: Some(0),
            java_enabled: Some(true),
            java_script_enabled: Some(true),
            ip_address: Some("127.0.0.1".parse().unwrap()),
            device_model: Some("Apple IPHONE 7".to_string()),
            os_type: Some("IOS or ANDROID".to_string()),
            os_version: Some("IOS 14.5".to_string()),
            accept_language: Some("en".to_string()),
        };
        Self(data)
    }
}

impl Default for PaymentSyncType {
    fn default() -> Self {
        let data = types::PaymentsSyncData {
            mandate_id: None,
            connector_transaction_id: types::ResponseId::ConnectorTransactionId(
                "12345".to_string(),
            ),
            encoded_data: None,
            capture_method: None,
            sync_type: types::SyncRequestType::SinglePaymentSync,
            connector_meta: None,
            payment_method_type: None,
            currency: enums::Currency::USD,
            payment_experience: None,
            amount: MinorUnit::new(100),
            integrity_object: None,
            ..Default::default()
        };
        Self(data)
    }
}

impl Default for PaymentRefundType {
    fn default() -> Self {
        let data = types::RefundsData {
            payment_amount: 100,
            minor_payment_amount: MinorUnit::new(100),
            currency: enums::Currency::USD,
            refund_id: uuid::Uuid::new_v4().to_string(),
            connector_transaction_id: String::new(),
            refund_amount: 100,
            minor_refund_amount: MinorUnit::new(100),
            webhook_url: None,
            connector_metadata: None,
            refund_connector_metadata: None,
            reason: Some("Customer returned product".to_string()),
            connector_refund_id: None,
            browser_info: None,
            split_refunds: None,
            integrity_object: None,
            refund_status: enums::RefundStatus::Pending,
            merchant_account_id: None,
            merchant_config_currency: None,
            capture_method: None,
            additional_payment_method_data: None,
        };
        Self(data)
    }
}

impl Default for CustomerType {
    fn default() -> Self {
        let data = types::ConnectorCustomerData {
            payment_method_data: Some(types::domain::PaymentMethodData::Card(
                CCardType::default().0,
            )),
            description: None,
            email: Email::from_str("test@juspay.in").ok(),
            phone: None,
            name: None,
            preprocessing_id: None,
            split_payments: None,
            customer_acceptance: None,
            setup_future_usage: None,
        };
        Self(data)
    }
}

impl Default for TokenType {
    fn default() -> Self {
        let data = types::PaymentMethodTokenizationData {
            payment_method_data: types::domain::PaymentMethodData::Card(CCardType::default().0),
            browser_info: None,
            amount: Some(100),
            currency: enums::Currency::USD,
            split_payments: None,
            mandate_id: None,
            setup_future_usage: None,
            customer_acceptance: None,
            setup_mandate_details: None,
        };
        Self(data)
    }
}

pub fn get_connector_transaction_id(
    response: Result<types::PaymentsResponseData, types::ErrorResponse>,
) -> Option<String> {
    match response {
        Ok(types::PaymentsResponseData::TransactionResponse { resource_id, .. }) => {
            resource_id.get_connector_transaction_id().ok()
        }
        Ok(types::PaymentsResponseData::SessionResponse { .. }) => None,
        Ok(types::PaymentsResponseData::SessionTokenResponse { .. }) => None,
        Ok(types::PaymentsResponseData::TokenizationResponse { .. }) => None,
        Ok(types::PaymentsResponseData::TransactionUnresolvedResponse { .. }) => None,
        Ok(types::PaymentsResponseData::PreProcessingResponse { .. }) => None,
        Ok(types::PaymentsResponseData::ConnectorCustomerResponse { .. }) => None,
        Ok(types::PaymentsResponseData::ThreeDSEnrollmentResponse { .. }) => None,
        Ok(types::PaymentsResponseData::MultipleCaptureResponse { .. }) => None,
        Ok(types::PaymentsResponseData::IncrementalAuthorizationResponse { .. }) => None,
        Ok(types::PaymentsResponseData::PostProcessingResponse { .. }) => None,
        Ok(types::PaymentsResponseData::PaymentResourceUpdateResponse { .. }) => None,
        Ok(types::PaymentsResponseData::PaymentsCreateOrderResponse { .. }) => None,
        Err(_) => None,
    }
}

pub fn get_connector_metadata(
    response: Result<types::PaymentsResponseData, types::ErrorResponse>,
) -> Option<serde_json::Value> {
    match response {
        Ok(types::PaymentsResponseData::TransactionResponse {
            resource_id: _,
            redirection_data: _,
            mandate_reference: _,
            connector_metadata,
            network_txn_id: _,
            connector_response_reference_id: _,
            incremental_authorization_allowed: _,
            charges: _,
        }) => connector_metadata,
        _ => None,
    }
}

pub fn to_connector_auth_type(auth_type: ConnectorAuthType) -> types::ConnectorAuthType {
    match auth_type {
        ConnectorAuthType::HeaderKey { api_key } => types::ConnectorAuthType::HeaderKey { api_key },
        ConnectorAuthType::BodyKey { api_key, key1 } => {
            types::ConnectorAuthType::BodyKey { api_key, key1 }
        }
        ConnectorAuthType::SignatureKey {
            api_key,
            key1,
            api_secret,
        } => types::ConnectorAuthType::SignatureKey {
            api_key,
            key1,
            api_secret,
        },
        ConnectorAuthType::MultiAuthKey {
            api_key,
            key1,
            api_secret,
            key2,
        } => types::ConnectorAuthType::MultiAuthKey {
            api_key,
            key1,
            api_secret,
            key2,
        },
        _ => types::ConnectorAuthType::NoKey,
    }
}
