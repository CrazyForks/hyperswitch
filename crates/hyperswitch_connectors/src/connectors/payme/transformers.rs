use std::collections::HashMap;

use api_models::enums::{AuthenticationType, PaymentMethod};
use common_enums::enums;
use common_utils::{
    pii,
    types::{MinorUnit, StringMajorUnit},
};
use error_stack::ResultExt;
use hyperswitch_domain_models::{
    payment_method_data::{PaymentMethodData, WalletData},
    router_data::{ConnectorAuthType, ErrorResponse, PaymentMethodToken, RouterData},
    router_flow_types::{Execute, Void},
    router_request_types::{PaymentsCancelData, PaymentsPreProcessingData, ResponseId},
    router_response_types::{
        MandateReference, PaymentsResponseData, PreprocessingResponseId, RedirectForm,
        RefundsResponseData,
    },
    types::{
        PaymentsAuthorizeRouterData, PaymentsCancelRouterData, PaymentsCaptureRouterData,
        PaymentsCompleteAuthorizeRouterData, PaymentsPreProcessingRouterData,
        PaymentsSyncRouterData, RefundSyncRouterData, RefundsRouterData, TokenizationRouterData,
    },
};
use hyperswitch_interfaces::{consts, errors};
use masking::{ExposeInterface, Secret};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    types::{PaymentsCancelResponseRouterData, RefundsResponseRouterData, ResponseRouterData},
    unimplemented_payment_method,
    utils::{
        self, AddressDetailsData, CardData, PaymentsAuthorizeRequestData,
        PaymentsCancelRequestData, PaymentsCompleteAuthorizeRequestData,
        PaymentsPreProcessingRequestData, PaymentsSyncRequestData, RouterData as OtherRouterData,
    },
};

const LANGUAGE: &str = "en";

#[derive(Debug, Serialize)]
pub struct PaymeRouterData<T> {
    pub amount: MinorUnit,
    pub router_data: T,
}

impl<T> TryFrom<(MinorUnit, T)> for PaymeRouterData<T> {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from((amount, item): (MinorUnit, T)) -> Result<Self, Self::Error> {
        Ok(Self {
            amount,
            router_data: item,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct PayRequest {
    buyer_name: Secret<String>,
    buyer_email: pii::Email,
    payme_sale_id: String,
    #[serde(flatten)]
    card: PaymeCard,
    language: String,
}

#[derive(Debug, Serialize)]
pub struct MandateRequest {
    currency: enums::Currency,
    sale_price: MinorUnit,
    transaction_id: String,
    product_name: String,
    sale_return_url: String,
    seller_payme_id: Secret<String>,
    sale_callback_url: String,
    buyer_key: Secret<String>,
    language: String,
}

#[derive(Debug, Serialize)]
pub struct Pay3dsRequest {
    buyer_name: Secret<String>,
    buyer_email: pii::Email,
    buyer_key: Secret<String>,
    payme_sale_id: String,
    meta_data_jwt: Secret<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PaymePaymentRequest {
    MandateRequest(MandateRequest),
    PayRequest(PayRequest),
}

#[derive(Debug, Serialize)]
pub struct PaymeQuerySaleRequest {
    sale_payme_id: String,
    seller_payme_id: Secret<String>,
}

#[derive(Debug, Serialize)]
pub struct PaymeQueryTransactionRequest {
    payme_transaction_id: String,
    seller_payme_id: Secret<String>,
}

#[derive(Debug, Serialize)]
pub struct PaymeCard {
    credit_card_cvv: Secret<String>,
    credit_card_exp: Secret<String>,
    credit_card_number: cards::CardNumber,
}

#[derive(Debug, Serialize)]
pub struct CaptureBuyerRequest {
    seller_payme_id: Secret<String>,
    #[serde(flatten)]
    card: PaymeCard,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CaptureBuyerResponse {
    buyer_key: Secret<String>,
}

#[derive(Debug, Serialize)]
pub struct GenerateSaleRequest {
    currency: enums::Currency,
    sale_type: SaleType,
    sale_price: MinorUnit,
    transaction_id: String,
    product_name: String,
    sale_return_url: String,
    seller_payme_id: Secret<String>,
    sale_callback_url: String,
    sale_payment_method: SalePaymentMethod,
    services: Option<ThreeDs>,
    language: String,
}

#[derive(Debug, Serialize)]
pub struct ThreeDs {
    name: ThreeDsType,
    settings: ThreeDsSettings,
}

#[derive(Debug, Serialize)]
pub enum ThreeDsType {
    #[serde(rename = "3D Secure")]
    ThreeDs,
}

#[derive(Debug, Serialize)]
pub struct ThreeDsSettings {
    active: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GenerateSaleResponse {
    payme_sale_id: String,
}

impl<F, T> TryFrom<ResponseRouterData<F, PaymePaymentsResponse, T, PaymentsResponseData>>
    for RouterData<F, T, PaymentsResponseData>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: ResponseRouterData<F, PaymePaymentsResponse, T, PaymentsResponseData>,
    ) -> Result<Self, Self::Error> {
        match item.response {
            // To handle webhook response
            PaymePaymentsResponse::PaymePaySaleResponse(response) => {
                Self::try_from(ResponseRouterData {
                    response,
                    data: item.data,
                    http_code: item.http_code,
                })
            }
            // To handle PSync response
            PaymePaymentsResponse::SaleQueryResponse(response) => {
                Self::try_from(ResponseRouterData {
                    response,
                    data: item.data,
                    http_code: item.http_code,
                })
            }
        }
    }
}

impl<F, T> TryFrom<ResponseRouterData<F, PaymePaySaleResponse, T, PaymentsResponseData>>
    for RouterData<F, T, PaymentsResponseData>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: ResponseRouterData<F, PaymePaySaleResponse, T, PaymentsResponseData>,
    ) -> Result<Self, Self::Error> {
        let status = enums::AttemptStatus::from(item.response.sale_status.clone());
        let response = if utils::is_payment_failure(status) {
            // To populate error message in case of failure
            Err(get_pay_sale_error_response((
                &item.response,
                item.http_code,
            )))
        } else {
            Ok(PaymentsResponseData::try_from(&item.response)?)
        };
        Ok(Self {
            status,
            response,
            ..item.data
        })
    }
}

fn get_pay_sale_error_response(
    (pay_sale_response, http_code): (&PaymePaySaleResponse, u16),
) -> ErrorResponse {
    let code = pay_sale_response
        .status_error_code
        .map(|error_code| error_code.to_string())
        .unwrap_or(consts::NO_ERROR_CODE.to_string());
    ErrorResponse {
        code,
        message: pay_sale_response
            .status_error_details
            .clone()
            .unwrap_or(consts::NO_ERROR_MESSAGE.to_string()),
        reason: pay_sale_response.status_error_details.to_owned(),
        status_code: http_code,
        attempt_status: None,
        connector_transaction_id: Some(pay_sale_response.payme_sale_id.clone()),
        network_advice_code: None,
        network_decline_code: None,
        network_error_message: None,
    }
}

impl TryFrom<&PaymePaySaleResponse> for PaymentsResponseData {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(value: &PaymePaySaleResponse) -> Result<Self, Self::Error> {
        let redirection_data = match value.sale_3ds {
            Some(true) => value.redirect_url.clone().map(|url| RedirectForm::Form {
                endpoint: url.to_string(),
                method: common_utils::request::Method::Get,
                form_fields: HashMap::<String, String>::new(),
            }),
            _ => None,
        };
        Ok(Self::TransactionResponse {
            resource_id: ResponseId::ConnectorTransactionId(value.payme_sale_id.clone()),
            redirection_data: Box::new(redirection_data),
            mandate_reference: Box::new(value.buyer_key.clone().map(|buyer_key| {
                MandateReference {
                    connector_mandate_id: Some(buyer_key.expose()),
                    payment_method_id: None,
                    mandate_metadata: None,
                    connector_mandate_request_reference_id: None,
                }
            })),
            connector_metadata: None,
            network_txn_id: None,
            connector_response_reference_id: None,
            incremental_authorization_allowed: None,
            charges: None,
        })
    }
}

impl<F, T> TryFrom<ResponseRouterData<F, SaleQueryResponse, T, PaymentsResponseData>>
    for RouterData<F, T, PaymentsResponseData>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: ResponseRouterData<F, SaleQueryResponse, T, PaymentsResponseData>,
    ) -> Result<Self, Self::Error> {
        // Only one element would be present since we are passing one transaction id in the PSync request
        let transaction_response = item
            .response
            .items
            .first()
            .cloned()
            .ok_or(errors::ConnectorError::ResponseHandlingFailed)?;
        let status = enums::AttemptStatus::from(transaction_response.sale_status.clone());
        let response = if utils::is_payment_failure(status) {
            // To populate error message in case of failure
            Err(get_sale_query_error_response((
                &transaction_response,
                item.http_code,
            )))
        } else {
            Ok(PaymentsResponseData::from(&transaction_response))
        };
        Ok(Self {
            status,
            response,
            ..item.data
        })
    }
}

fn get_sale_query_error_response(
    (sale_query_response, http_code): (&SaleQuery, u16),
) -> ErrorResponse {
    ErrorResponse {
        code: sale_query_response
            .sale_error_code
            .clone()
            .unwrap_or(consts::NO_ERROR_CODE.to_string()),
        message: sale_query_response
            .sale_error_text
            .clone()
            .unwrap_or(consts::NO_ERROR_MESSAGE.to_string()),
        reason: sale_query_response.sale_error_text.clone(),
        status_code: http_code,
        attempt_status: None,
        connector_transaction_id: Some(sale_query_response.sale_payme_id.clone()),
        network_advice_code: None,
        network_decline_code: None,
        network_error_message: None,
    }
}

impl From<&SaleQuery> for PaymentsResponseData {
    fn from(value: &SaleQuery) -> Self {
        Self::TransactionResponse {
            resource_id: ResponseId::ConnectorTransactionId(value.sale_payme_id.clone()),
            redirection_data: Box::new(None),
            // mandate reference will be updated with webhooks only. That has been handled with PaymePaySaleResponse struct
            mandate_reference: Box::new(None),
            connector_metadata: None,
            network_txn_id: None,
            connector_response_reference_id: None,
            incremental_authorization_allowed: None,
            charges: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SaleType {
    Sale,
    Authorize,
    Token,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SalePaymentMethod {
    CreditCard,
    ApplePay,
}

impl TryFrom<&PaymeRouterData<&PaymentsPreProcessingRouterData>> for GenerateSaleRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: &PaymeRouterData<&PaymentsPreProcessingRouterData>,
    ) -> Result<Self, Self::Error> {
        let sale_type = SaleType::try_from(item.router_data)?;
        let seller_payme_id =
            PaymeAuthType::try_from(&item.router_data.connector_auth_type)?.seller_payme_id;
        let order_details = item.router_data.request.get_order_details()?;
        let services = get_services(item.router_data);
        let product_name = order_details
            .first()
            .ok_or_else(utils::missing_field_err("order_details"))?
            .product_name
            .clone();
        let pmd = item
            .router_data
            .request
            .payment_method_data
            .to_owned()
            .ok_or_else(utils::missing_field_err("payment_method_data"))?;
        Ok(Self {
            seller_payme_id,
            sale_price: item.amount.to_owned(),
            currency: item.router_data.request.get_currency()?,
            product_name,
            sale_payment_method: SalePaymentMethod::try_from(&pmd)?,
            sale_type,
            transaction_id: item.router_data.payment_id.clone(),
            sale_return_url: item.router_data.request.get_router_return_url()?,
            sale_callback_url: item.router_data.request.get_webhook_url()?,
            language: LANGUAGE.to_string(),
            services,
        })
    }
}

impl TryFrom<&PaymentMethodData> for SalePaymentMethod {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &PaymentMethodData) -> Result<Self, Self::Error> {
        match item {
            PaymentMethodData::Card(_) => Ok(Self::CreditCard),
            PaymentMethodData::Wallet(wallet_data) => match wallet_data {
                WalletData::ApplePayThirdPartySdk(_) => Ok(Self::ApplePay),
                WalletData::AliPayQr(_)
                | WalletData::AliPayRedirect(_)
                | WalletData::BluecodeRedirect {}
                | WalletData::AliPayHkRedirect(_)
                | WalletData::AmazonPayRedirect(_)
                | WalletData::Paysera(_)
                | WalletData::Skrill(_)
                | WalletData::MomoRedirect(_)
                | WalletData::KakaoPayRedirect(_)
                | WalletData::GoPayRedirect(_)
                | WalletData::GcashRedirect(_)
                | WalletData::ApplePayRedirect(_)
                | WalletData::DanaRedirect {}
                | WalletData::GooglePay(_)
                | WalletData::GooglePayRedirect(_)
                | WalletData::GooglePayThirdPartySdk(_)
                | WalletData::MbWayRedirect(_)
                | WalletData::MobilePayRedirect(_)
                | WalletData::PaypalRedirect(_)
                | WalletData::PaypalSdk(_)
                | WalletData::Paze(_)
                | WalletData::SamsungPay(_)
                | WalletData::TwintRedirect {}
                | WalletData::VippsRedirect {}
                | WalletData::TouchNGoRedirect(_)
                | WalletData::WeChatPayRedirect(_)
                | WalletData::WeChatPayQr(_)
                | WalletData::CashappQr(_)
                | WalletData::ApplePay(_)
                | WalletData::SwishQr(_)
                | WalletData::Mifinity(_)
                | WalletData::RevolutPay(_) => Err(errors::ConnectorError::NotSupported {
                    message: "Wallet".to_string(),
                    connector: "payme",
                }
                .into()),
            },
            PaymentMethodData::PayLater(_)
            | PaymentMethodData::BankRedirect(_)
            | PaymentMethodData::BankDebit(_)
            | PaymentMethodData::BankTransfer(_)
            | PaymentMethodData::Crypto(_)
            | PaymentMethodData::MandatePayment
            | PaymentMethodData::Reward
            | PaymentMethodData::RealTimePayment(_)
            | PaymentMethodData::MobilePayment(_)
            | PaymentMethodData::GiftCard(_)
            | PaymentMethodData::CardRedirect(_)
            | PaymentMethodData::Upi(_)
            | PaymentMethodData::Voucher(_)
            | PaymentMethodData::OpenBanking(_)
            | PaymentMethodData::CardToken(_)
            | PaymentMethodData::NetworkToken(_)
            | PaymentMethodData::CardDetailsForNetworkTransactionId(_) => {
                Err(errors::ConnectorError::NotImplemented("Payment methods".to_string()).into())
            }
        }
    }
}

impl TryFrom<&PaymeRouterData<&PaymentsAuthorizeRouterData>> for PaymePaymentRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        value: &PaymeRouterData<&PaymentsAuthorizeRouterData>,
    ) -> Result<Self, Self::Error> {
        let payme_request = if value.router_data.request.mandate_id.is_some() {
            Self::MandateRequest(MandateRequest::try_from(value)?)
        } else {
            Self::PayRequest(PayRequest::try_from(value.router_data)?)
        };
        Ok(payme_request)
    }
}

impl TryFrom<&PaymentsSyncRouterData> for PaymeQuerySaleRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(value: &PaymentsSyncRouterData) -> Result<Self, Self::Error> {
        let seller_payme_id = PaymeAuthType::try_from(&value.connector_auth_type)?.seller_payme_id;
        Ok(Self {
            sale_payme_id: value.request.get_connector_transaction_id()?,
            seller_payme_id,
        })
    }
}

impl TryFrom<&RefundSyncRouterData> for PaymeQueryTransactionRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(value: &RefundSyncRouterData) -> Result<Self, Self::Error> {
        let seller_payme_id = PaymeAuthType::try_from(&value.connector_auth_type)?.seller_payme_id;
        Ok(Self {
            payme_transaction_id: value
                .request
                .connector_refund_id
                .clone()
                .ok_or(errors::ConnectorError::MissingConnectorRefundID)?,
            seller_payme_id,
        })
    }
}

impl<F>
    utils::ForeignTryFrom<(
        ResponseRouterData<
            F,
            GenerateSaleResponse,
            PaymentsPreProcessingData,
            PaymentsResponseData,
        >,
        StringMajorUnit,
    )> for RouterData<F, PaymentsPreProcessingData, PaymentsResponseData>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn foreign_try_from(
        (item, apple_pay_amount): (
            ResponseRouterData<
                F,
                GenerateSaleResponse,
                PaymentsPreProcessingData,
                PaymentsResponseData,
            >,
            StringMajorUnit,
        ),
    ) -> Result<Self, Self::Error> {
        match item.data.payment_method {
            PaymentMethod::Card => {
                match item.data.auth_type {
                    AuthenticationType::NoThreeDs => {
                        Ok(Self {
                            // We don't get any status from payme, so defaulting it to pending
                            // then move to authorize flow
                            status: enums::AttemptStatus::Pending,
                            preprocessing_id: Some(item.response.payme_sale_id.to_owned()),
                            response: Ok(PaymentsResponseData::PreProcessingResponse {
                                pre_processing_id: PreprocessingResponseId::ConnectorTransactionId(
                                    item.response.payme_sale_id,
                                ),
                                connector_metadata: None,
                                session_token: None,
                                connector_response_reference_id: None,
                            }),
                            ..item.data
                        })
                    }
                    AuthenticationType::ThreeDs => Ok(Self {
                        // We don't go to authorize flow in 3ds,
                        // Response is send directly after preprocessing flow
                        // redirection data is send to run script along
                        // status is made authentication_pending to show redirection
                        status: enums::AttemptStatus::AuthenticationPending,
                        preprocessing_id: Some(item.response.payme_sale_id.to_owned()),
                        response: Ok(PaymentsResponseData::TransactionResponse {
                            resource_id: ResponseId::ConnectorTransactionId(
                                item.response.payme_sale_id.to_owned(),
                            ),
                            redirection_data: Box::new(Some(RedirectForm::Payme)),
                            mandate_reference: Box::new(None),
                            connector_metadata: None,
                            network_txn_id: None,
                            connector_response_reference_id: None,
                            incremental_authorization_allowed: None,
                            charges: None,
                        }),
                        ..item.data
                    }),
                }
            }
            _ => {
                let currency_code = item.data.request.get_currency()?;
                let pmd = item.data.request.payment_method_data.to_owned();
                let payme_auth_type = PaymeAuthType::try_from(&item.data.connector_auth_type)?;

                let session_token = match pmd {
                    Some(PaymentMethodData::Wallet(WalletData::ApplePayThirdPartySdk(
                        _,
                    ))) => Some(api_models::payments::SessionToken::ApplePay(Box::new(
                        api_models::payments::ApplepaySessionTokenResponse {
                            session_token_data: Some(
                                api_models::payments::ApplePaySessionResponse::NoSessionResponse(api_models::payments::NullObject),
                            ),
                            payment_request_data: Some(
                                api_models::payments::ApplePayPaymentRequest {
                                    country_code: item.data.get_billing_country()?,
                                    currency_code,
                                    total: api_models::payments::AmountInfo {
                                        label: "Apple Pay".to_string(),
                                        total_type: None,
                                        amount: apple_pay_amount,
                                    },
                                    merchant_capabilities: None,
                                    supported_networks: None,
                                    merchant_identifier: None,
                                    required_billing_contact_fields: None,
                                    required_shipping_contact_fields: None,
                                    recurring_payment_request: None,
                                },
                            ),
                            connector: "payme".to_string(),
                            delayed_session_token: true,
                            sdk_next_action: api_models::payments::SdkNextAction {
                                next_action: api_models::payments::NextActionCall::Sync,
                            },
                            connector_reference_id: Some(item.response.payme_sale_id.to_owned()),
                            connector_sdk_public_key: Some(
                                payme_auth_type.payme_public_key.expose(),
                            ),
                            connector_merchant_id: payme_auth_type
                                .payme_merchant_id
                                .map(|mid| mid.expose()),
                        },
                    ))),
                    _ => None,
                };
                Ok(Self {
                    // We don't get any status from payme, so defaulting it to pending
                    status: enums::AttemptStatus::Pending,
                    preprocessing_id: Some(item.response.payme_sale_id.to_owned()),
                    response: Ok(PaymentsResponseData::PreProcessingResponse {
                        pre_processing_id: PreprocessingResponseId::ConnectorTransactionId(
                            item.response.payme_sale_id,
                        ),
                        connector_metadata: None,
                        session_token,
                        connector_response_reference_id: None,
                    }),
                    ..item.data
                })
            }
        }
    }
}

impl TryFrom<&PaymeRouterData<&PaymentsAuthorizeRouterData>> for MandateRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &PaymeRouterData<&PaymentsAuthorizeRouterData>) -> Result<Self, Self::Error> {
        let seller_payme_id =
            PaymeAuthType::try_from(&item.router_data.connector_auth_type)?.seller_payme_id;
        let order_details = item.router_data.request.get_order_details()?;
        let product_name = order_details
            .first()
            .ok_or_else(utils::missing_field_err("order_details"))?
            .product_name
            .clone();
        Ok(Self {
            currency: item.router_data.request.currency,
            sale_price: item.amount.to_owned(),
            transaction_id: item.router_data.payment_id.clone(),
            product_name,
            sale_return_url: item.router_data.request.get_router_return_url()?,
            seller_payme_id,
            sale_callback_url: item.router_data.request.get_webhook_url()?,
            buyer_key: Secret::new(item.router_data.request.get_connector_mandate_id()?),
            language: LANGUAGE.to_string(),
        })
    }
}

impl TryFrom<&PaymentsAuthorizeRouterData> for PayRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &PaymentsAuthorizeRouterData) -> Result<Self, Self::Error> {
        match item.request.payment_method_data.clone() {
            PaymentMethodData::Card(req_card) => {
                let card = PaymeCard {
                    credit_card_cvv: req_card.card_cvc.clone(),
                    credit_card_exp: req_card
                        .get_card_expiry_month_year_2_digit_with_delimiter("".to_string())?,
                    credit_card_number: req_card.card_number,
                };
                let buyer_email = item.request.get_email()?;
                let buyer_name = item.get_billing_address()?.get_full_name()?;
                let payme_sale_id = item.preprocessing_id.to_owned().ok_or(
                    errors::ConnectorError::MissingConnectorRelatedTransactionID {
                        id: "payme_sale_id".to_string(),
                    },
                )?;
                Ok(Self {
                    card,
                    buyer_email,
                    buyer_name,
                    payme_sale_id,
                    language: LANGUAGE.to_string(),
                })
            }
            PaymentMethodData::CardRedirect(_)
            | PaymentMethodData::Wallet(_)
            | PaymentMethodData::PayLater(_)
            | PaymentMethodData::BankRedirect(_)
            | PaymentMethodData::BankDebit(_)
            | PaymentMethodData::BankTransfer(_)
            | PaymentMethodData::Crypto(_)
            | PaymentMethodData::MandatePayment
            | PaymentMethodData::Reward
            | PaymentMethodData::RealTimePayment(_)
            | PaymentMethodData::MobilePayment(_)
            | PaymentMethodData::Upi(_)
            | PaymentMethodData::Voucher(_)
            | PaymentMethodData::GiftCard(_)
            | PaymentMethodData::OpenBanking(_)
            | PaymentMethodData::CardToken(_)
            | PaymentMethodData::NetworkToken(_)
            | PaymentMethodData::CardDetailsForNetworkTransactionId(_) => {
                Err(errors::ConnectorError::NotImplemented(
                    utils::get_unimplemented_payment_method_error_message("payme"),
                ))?
            }
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymeRedirectResponseData {
    meta_data: String,
}

impl TryFrom<&PaymentsCompleteAuthorizeRouterData> for Pay3dsRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &PaymentsCompleteAuthorizeRouterData) -> Result<Self, Self::Error> {
        match item.request.payment_method_data.clone() {
            Some(PaymentMethodData::Card(_)) => {
                let buyer_email = item.request.get_email()?;
                let buyer_name = item.get_billing_address()?.get_full_name()?;

                let payload_data = item.request.get_redirect_response_payload()?.expose();

                let jwt_data: PaymeRedirectResponseData = serde_json::from_value(payload_data)
                    .change_context(errors::ConnectorError::MissingConnectorRedirectionPayload {
                        field_name: "meta_data_jwt",
                    })?;

                let payme_sale_id = item
                    .request
                    .connector_transaction_id
                    .clone()
                    .ok_or(errors::ConnectorError::MissingConnectorTransactionID)?;
                let pm_token = item.get_payment_method_token()?;
                let buyer_key =
                    match pm_token {
                        PaymentMethodToken::Token(token) => token,
                        PaymentMethodToken::ApplePayDecrypt(_) => Err(
                            unimplemented_payment_method!("Apple Pay", "Simplified", "Payme"),
                        )?,
                        PaymentMethodToken::PazeDecrypt(_) => {
                            Err(unimplemented_payment_method!("Paze", "Payme"))?
                        }
                        PaymentMethodToken::GooglePayDecrypt(_) => {
                            Err(unimplemented_payment_method!("Google Pay", "Payme"))?
                        }
                    };
                Ok(Self {
                    buyer_email,
                    buyer_key,
                    buyer_name,
                    payme_sale_id,
                    meta_data_jwt: Secret::new(jwt_data.meta_data),
                })
            }
            Some(PaymentMethodData::CardRedirect(_))
            | Some(PaymentMethodData::Wallet(_))
            | Some(PaymentMethodData::PayLater(_))
            | Some(PaymentMethodData::BankRedirect(_))
            | Some(PaymentMethodData::BankDebit(_))
            | Some(PaymentMethodData::BankTransfer(_))
            | Some(PaymentMethodData::Crypto(_))
            | Some(PaymentMethodData::MandatePayment)
            | Some(PaymentMethodData::Reward)
            | Some(PaymentMethodData::RealTimePayment(_))
            | Some(PaymentMethodData::MobilePayment(_))
            | Some(PaymentMethodData::Upi(_))
            | Some(PaymentMethodData::Voucher(_))
            | Some(PaymentMethodData::GiftCard(_))
            | Some(PaymentMethodData::OpenBanking(_))
            | Some(PaymentMethodData::CardToken(_))
            | Some(PaymentMethodData::NetworkToken(_))
            | Some(PaymentMethodData::CardDetailsForNetworkTransactionId(_))
            | None => {
                Err(errors::ConnectorError::NotImplemented("Tokenize Flow".to_string()).into())
            }
        }
    }
}

impl TryFrom<&TokenizationRouterData> for CaptureBuyerRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &TokenizationRouterData) -> Result<Self, Self::Error> {
        match item.request.payment_method_data.clone() {
            PaymentMethodData::Card(req_card) => {
                let seller_payme_id =
                    PaymeAuthType::try_from(&item.connector_auth_type)?.seller_payme_id;
                let card = PaymeCard {
                    credit_card_cvv: req_card.card_cvc.clone(),
                    credit_card_exp: req_card
                        .get_card_expiry_month_year_2_digit_with_delimiter("".to_string())?,
                    credit_card_number: req_card.card_number,
                };
                Ok(Self {
                    card,
                    seller_payme_id,
                })
            }
            PaymentMethodData::Wallet(_)
            | PaymentMethodData::CardRedirect(_)
            | PaymentMethodData::PayLater(_)
            | PaymentMethodData::BankRedirect(_)
            | PaymentMethodData::BankDebit(_)
            | PaymentMethodData::BankTransfer(_)
            | PaymentMethodData::Crypto(_)
            | PaymentMethodData::MandatePayment
            | PaymentMethodData::Reward
            | PaymentMethodData::RealTimePayment(_)
            | PaymentMethodData::MobilePayment(_)
            | PaymentMethodData::Upi(_)
            | PaymentMethodData::Voucher(_)
            | PaymentMethodData::GiftCard(_)
            | PaymentMethodData::OpenBanking(_)
            | PaymentMethodData::CardToken(_)
            | PaymentMethodData::NetworkToken(_)
            | PaymentMethodData::CardDetailsForNetworkTransactionId(_) => {
                Err(errors::ConnectorError::NotImplemented("Tokenize Flow".to_string()).into())
            }
        }
    }
}

// Auth Struct
pub struct PaymeAuthType {
    #[allow(dead_code)]
    pub(super) payme_public_key: Secret<String>,
    pub(super) seller_payme_id: Secret<String>,
    pub(super) payme_merchant_id: Option<Secret<String>>,
}

impl TryFrom<&ConnectorAuthType> for PaymeAuthType {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(auth_type: &ConnectorAuthType) -> Result<Self, Self::Error> {
        match auth_type {
            ConnectorAuthType::BodyKey { api_key, key1 } => Ok(Self {
                seller_payme_id: api_key.to_owned(),
                payme_public_key: key1.to_owned(),
                payme_merchant_id: None,
            }),
            ConnectorAuthType::SignatureKey {
                api_key,
                key1,
                api_secret,
            } => Ok(Self {
                seller_payme_id: api_key.to_owned(),
                payme_public_key: key1.to_owned(),
                payme_merchant_id: Some(api_secret.to_owned()),
            }),
            _ => Err(errors::ConnectorError::FailedToObtainAuthType.into()),
        }
    }
}

impl TryFrom<&PaymentsPreProcessingRouterData> for SaleType {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(value: &PaymentsPreProcessingRouterData) -> Result<Self, Self::Error> {
        let sale_type = if value.request.setup_mandate_details.is_some() {
            // First mandate
            Self::Token
        } else {
            // Normal payments
            match value.request.is_auto_capture()? {
                true => Self::Sale,
                false => Self::Authorize,
            }
        };
        Ok(sale_type)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, strum::Display)]
#[serde(rename_all = "kebab-case")]
pub enum SaleStatus {
    Initial,
    Completed,
    Refunded,
    PartialRefund,
    Authorized,
    Voided,
    PartialVoid,
    Failed,
    Chargeback,
}

impl From<SaleStatus> for enums::AttemptStatus {
    fn from(item: SaleStatus) -> Self {
        match item {
            SaleStatus::Initial => Self::Authorizing,
            SaleStatus::Completed => Self::Charged,
            SaleStatus::Refunded | SaleStatus::PartialRefund => Self::AutoRefunded,
            SaleStatus::Authorized => Self::Authorized,
            SaleStatus::Voided | SaleStatus::PartialVoid => Self::Voided,
            SaleStatus::Failed => Self::Failure,
            SaleStatus::Chargeback => Self::AutoRefunded,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PaymePaymentsResponse {
    PaymePaySaleResponse(PaymePaySaleResponse),
    SaleQueryResponse(SaleQueryResponse),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SaleQueryResponse {
    items: Vec<SaleQuery>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SaleQuery {
    sale_status: SaleStatus,
    sale_payme_id: String,
    sale_error_text: Option<String>,
    sale_error_code: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymePaySaleResponse {
    sale_status: SaleStatus,
    payme_sale_id: String,
    payme_transaction_id: Option<String>,
    buyer_key: Option<Secret<String>>,
    status_error_details: Option<String>,
    status_error_code: Option<u32>,
    sale_3ds: Option<bool>,
    redirect_url: Option<Url>,
}

#[derive(Serialize, Deserialize)]
pub struct PaymeMetadata {
    payme_transaction_id: Option<String>,
}

impl<F, T> TryFrom<ResponseRouterData<F, CaptureBuyerResponse, T, PaymentsResponseData>>
    for RouterData<F, T, PaymentsResponseData>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: ResponseRouterData<F, CaptureBuyerResponse, T, PaymentsResponseData>,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            payment_method_token: Some(PaymentMethodToken::Token(item.response.buyer_key.clone())),
            response: Ok(PaymentsResponseData::TokenizationResponse {
                token: item.response.buyer_key.expose(),
            }),
            ..item.data
        })
    }
}

#[derive(Debug, Serialize)]
pub struct PaymentCaptureRequest {
    payme_sale_id: String,
    sale_price: MinorUnit,
}

impl TryFrom<&PaymeRouterData<&PaymentsCaptureRouterData>> for PaymentCaptureRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &PaymeRouterData<&PaymentsCaptureRouterData>) -> Result<Self, Self::Error> {
        if item.router_data.request.minor_amount_to_capture
            != item.router_data.request.minor_payment_amount
        {
            Err(errors::ConnectorError::NotSupported {
                message: "Partial Capture".to_string(),
                connector: "Payme",
            })?
        }
        Ok(Self {
            payme_sale_id: item.router_data.request.connector_transaction_id.clone(),
            sale_price: item.amount,
        })
    }
}

// REFUND :
// Type definition for RefundRequest
#[derive(Debug, Serialize)]
pub struct PaymeRefundRequest {
    sale_refund_amount: MinorUnit,
    payme_sale_id: String,
    seller_payme_id: Secret<String>,
    language: String,
}

impl<F> TryFrom<&PaymeRouterData<&RefundsRouterData<F>>> for PaymeRefundRequest {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(item: &PaymeRouterData<&RefundsRouterData<F>>) -> Result<Self, Self::Error> {
        let auth_type = PaymeAuthType::try_from(&item.router_data.connector_auth_type)?;
        Ok(Self {
            payme_sale_id: item.router_data.request.connector_transaction_id.clone(),
            seller_payme_id: auth_type.seller_payme_id,
            sale_refund_amount: item.amount.to_owned(),
            language: LANGUAGE.to_string(),
        })
    }
}

impl TryFrom<SaleStatus> for enums::RefundStatus {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(sale_status: SaleStatus) -> Result<Self, Self::Error> {
        match sale_status {
            SaleStatus::Refunded | SaleStatus::PartialRefund => Ok(Self::Success),
            SaleStatus::Failed => Ok(Self::Failure),
            SaleStatus::Initial
            | SaleStatus::Completed
            | SaleStatus::Authorized
            | SaleStatus::Voided
            | SaleStatus::PartialVoid
            | SaleStatus::Chargeback => Err(errors::ConnectorError::ResponseHandlingFailed)?,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PaymeRefundResponse {
    sale_status: SaleStatus,
    payme_transaction_id: Option<String>,
    status_error_code: Option<u32>,
}

impl TryFrom<RefundsResponseRouterData<Execute, PaymeRefundResponse>>
    for RefundsRouterData<Execute>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: RefundsResponseRouterData<Execute, PaymeRefundResponse>,
    ) -> Result<Self, Self::Error> {
        let refund_status = enums::RefundStatus::try_from(item.response.sale_status.clone())?;
        let response = if utils::is_refund_failure(refund_status) {
            let payme_response = &item.response;
            let status_error_code = payme_response
                .status_error_code
                .map(|error_code| error_code.to_string());
            Err(ErrorResponse {
                code: status_error_code
                    .clone()
                    .unwrap_or_else(|| consts::NO_ERROR_CODE.to_string()),
                message: status_error_code
                    .clone()
                    .unwrap_or_else(|| consts::NO_ERROR_MESSAGE.to_string()),
                reason: status_error_code,
                status_code: item.http_code,
                attempt_status: None,
                connector_transaction_id: payme_response.payme_transaction_id.clone(),
                network_advice_code: None,
                network_decline_code: None,
                network_error_message: None,
            })
        } else {
            Ok(RefundsResponseData {
                connector_refund_id: item
                    .response
                    .payme_transaction_id
                    .ok_or(errors::ConnectorError::MissingConnectorRefundID)?,
                refund_status,
            })
        };
        Ok(Self {
            response,
            ..item.data
        })
    }
}

#[derive(Debug, Serialize)]
pub struct PaymeVoidRequest {
    sale_currency: enums::Currency,
    payme_sale_id: String,
    seller_payme_id: Secret<String>,
    language: String,
}

impl TryFrom<&PaymeRouterData<&RouterData<Void, PaymentsCancelData, PaymentsResponseData>>>
    for PaymeVoidRequest
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: &PaymeRouterData<&RouterData<Void, PaymentsCancelData, PaymentsResponseData>>,
    ) -> Result<Self, Self::Error> {
        let auth_type = PaymeAuthType::try_from(&item.router_data.connector_auth_type)?;
        Ok(Self {
            payme_sale_id: item.router_data.request.connector_transaction_id.clone(),
            seller_payme_id: auth_type.seller_payme_id,
            sale_currency: item.router_data.request.get_currency()?,
            language: LANGUAGE.to_string(),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PaymeVoidResponse {
    sale_status: SaleStatus,
    payme_transaction_id: Option<String>,
    status_error_code: Option<u32>,
}

impl TryFrom<PaymentsCancelResponseRouterData<PaymeVoidResponse>> for PaymentsCancelRouterData {
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: PaymentsCancelResponseRouterData<PaymeVoidResponse>,
    ) -> Result<Self, Self::Error> {
        let status = enums::AttemptStatus::from(item.response.sale_status.clone());
        let response = if utils::is_payment_failure(status) {
            let payme_response = &item.response;
            let status_error_code = payme_response
                .status_error_code
                .map(|error_code| error_code.to_string());
            Err(ErrorResponse {
                code: status_error_code
                    .clone()
                    .unwrap_or_else(|| consts::NO_ERROR_CODE.to_string()),
                message: status_error_code
                    .clone()
                    .unwrap_or_else(|| consts::NO_ERROR_MESSAGE.to_string()),
                reason: status_error_code,
                status_code: item.http_code,
                attempt_status: None,
                connector_transaction_id: payme_response.payme_transaction_id.clone(),
                network_advice_code: None,
                network_decline_code: None,
                network_error_message: None,
            })
        } else {
            // Since we are not receiving payme_sale_id, we are not populating the transaction response
            Ok(PaymentsResponseData::TransactionResponse {
                resource_id: ResponseId::NoResponseId,
                redirection_data: Box::new(None),
                mandate_reference: Box::new(None),
                connector_metadata: None,
                network_txn_id: None,
                connector_response_reference_id: None,
                incremental_authorization_allowed: None,
                charges: None,
            })
        };
        Ok(Self {
            status,
            response,
            ..item.data
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymeQueryTransactionResponse {
    items: Vec<TransactionQuery>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransactionQuery {
    sale_status: SaleStatus,
    payme_transaction_id: String,
}

impl<F, T> TryFrom<ResponseRouterData<F, PaymeQueryTransactionResponse, T, RefundsResponseData>>
    for RouterData<F, T, RefundsResponseData>
{
    type Error = error_stack::Report<errors::ConnectorError>;
    fn try_from(
        item: ResponseRouterData<F, PaymeQueryTransactionResponse, T, RefundsResponseData>,
    ) -> Result<Self, Self::Error> {
        let pay_sale_response = item
            .response
            .items
            .first()
            .ok_or(errors::ConnectorError::ResponseHandlingFailed)?;
        let refund_status = enums::RefundStatus::try_from(pay_sale_response.sale_status.clone())?;
        let response = if utils::is_refund_failure(refund_status) {
            Err(ErrorResponse {
                code: consts::NO_ERROR_CODE.to_string(),
                message: consts::NO_ERROR_CODE.to_string(),
                reason: None,
                status_code: item.http_code,
                attempt_status: None,
                connector_transaction_id: Some(pay_sale_response.payme_transaction_id.clone()),
                network_advice_code: None,
                network_decline_code: None,
                network_error_message: None,
            })
        } else {
            Ok(RefundsResponseData {
                refund_status,
                connector_refund_id: pay_sale_response.payme_transaction_id.clone(),
            })
        };
        Ok(Self {
            response,
            ..item.data
        })
    }
}

fn get_services(item: &PaymentsPreProcessingRouterData) -> Option<ThreeDs> {
    match item.auth_type {
        AuthenticationType::ThreeDs => {
            let settings = ThreeDsSettings { active: true };
            Some(ThreeDs {
                name: ThreeDsType::ThreeDs,
                settings,
            })
        }
        AuthenticationType::NoThreeDs => None,
    }
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct PaymeErrorResponse {
    pub status_code: u16,
    pub status_error_details: String,
    pub status_additional_info: serde_json::Value,
    pub status_error_code: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyType {
    SaleComplete,
    SaleAuthorized,
    Refund,
    SaleFailure,
    SaleChargeback,
    SaleChargebackRefund,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookEventDataResource {
    pub sale_status: SaleStatus,
    pub payme_signature: Secret<String>,
    pub buyer_key: Option<Secret<String>>,
    pub notify_type: NotifyType,
    pub payme_sale_id: String,
    pub payme_transaction_id: String,
    pub status_error_details: Option<String>,
    pub status_error_code: Option<u32>,
    pub price: MinorUnit,
    pub currency: enums::Currency,
}

#[derive(Debug, Deserialize)]
pub struct WebhookEventDataResourceEvent {
    pub notify_type: NotifyType,
}

#[derive(Debug, Deserialize)]
pub struct WebhookEventDataResourceSignature {
    pub payme_signature: Secret<String>,
}

/// This try_from will ensure that webhook body would be properly parsed into PSync response
impl From<WebhookEventDataResource> for PaymePaySaleResponse {
    fn from(value: WebhookEventDataResource) -> Self {
        Self {
            sale_status: value.sale_status,
            payme_sale_id: value.payme_sale_id,
            payme_transaction_id: Some(value.payme_transaction_id),
            buyer_key: value.buyer_key,
            sale_3ds: None,
            redirect_url: None,
            status_error_code: value.status_error_code,
            status_error_details: value.status_error_details,
        }
    }
}

/// This try_from will ensure that webhook body would be properly parsed into RSync response
impl From<WebhookEventDataResource> for PaymeQueryTransactionResponse {
    fn from(value: WebhookEventDataResource) -> Self {
        let item = TransactionQuery {
            sale_status: value.sale_status,
            payme_transaction_id: value.payme_transaction_id,
        };
        Self { items: vec![item] }
    }
}

impl From<NotifyType> for api_models::webhooks::IncomingWebhookEvent {
    fn from(value: NotifyType) -> Self {
        match value {
            NotifyType::SaleComplete => Self::PaymentIntentSuccess,
            NotifyType::Refund => Self::RefundSuccess,
            NotifyType::SaleFailure => Self::PaymentIntentFailure,
            NotifyType::SaleChargeback => Self::DisputeOpened,
            NotifyType::SaleChargebackRefund => Self::DisputeWon,
            NotifyType::SaleAuthorized => Self::EventNotSupported,
        }
    }
}
