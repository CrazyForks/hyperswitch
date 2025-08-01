use common_utils::ext_traits::AsyncExt;
use error_stack::ResultExt;
use hyperswitch_domain_models::router_data_v2::ExternalAuthenticationFlowData;
use masking::ExposeInterface;

use crate::{
    consts,
    core::{
        errors::{self, ConnectorErrorExt, StorageErrorExt},
        payments,
    },
    errors::RouterResult,
    routes::SessionState,
    services::{self, execute_connector_processing_step},
    types::{
        api, authentication::AuthenticationResponseData, domain, storage,
        transformers::ForeignFrom, RouterData,
    },
    utils::OptionExt,
};

#[cfg(feature = "v1")]
pub fn get_connector_data_if_separate_authn_supported(
    connector_call_type: &api::ConnectorCallType,
) -> Option<api::ConnectorData> {
    match connector_call_type {
        api::ConnectorCallType::PreDetermined(connector_routing_data) => {
            if connector_routing_data
                .connector_data
                .connector_name
                .is_separate_authentication_supported()
            {
                Some(connector_routing_data.connector_data.clone())
            } else {
                None
            }
        }
        api::ConnectorCallType::Retryable(connector_routing_data) => connector_routing_data
            .first()
            .and_then(|connector_routing_data| {
                if connector_routing_data
                    .connector_data
                    .connector_name
                    .is_separate_authentication_supported()
                {
                    Some(connector_routing_data.connector_data.clone())
                } else {
                    None
                }
            }),
        api::ConnectorCallType::SessionMultiple(_) => None,
    }
}

pub async fn update_trackers<F: Clone, Req>(
    state: &SessionState,
    router_data: RouterData<F, Req, AuthenticationResponseData>,
    authentication: storage::Authentication,
    acquirer_details: Option<super::types::AcquirerDetails>,
    merchant_key_store: &hyperswitch_domain_models::merchant_key_store::MerchantKeyStore,
) -> RouterResult<storage::Authentication> {
    let authentication_update = match router_data.response {
        Ok(response) => match response {
            AuthenticationResponseData::PreAuthNResponse {
                threeds_server_transaction_id,
                maximum_supported_3ds_version,
                connector_authentication_id,
                three_ds_method_data,
                three_ds_method_url,
                message_version,
                connector_metadata,
                directory_server_id,
            } => storage::AuthenticationUpdate::PreAuthenticationUpdate {
                threeds_server_transaction_id,
                maximum_supported_3ds_version,
                connector_authentication_id,
                three_ds_method_data,
                three_ds_method_url,
                message_version,
                connector_metadata,
                authentication_status: common_enums::AuthenticationStatus::Pending,
                acquirer_bin: acquirer_details
                    .as_ref()
                    .map(|acquirer_details| acquirer_details.acquirer_bin.clone()),
                acquirer_merchant_id: acquirer_details
                    .as_ref()
                    .map(|acquirer_details| acquirer_details.acquirer_merchant_id.clone()),
                acquirer_country_code: acquirer_details
                    .and_then(|acquirer_details| acquirer_details.acquirer_country_code),
                directory_server_id,
                billing_address: None,
                shipping_address: None,
                browser_info: Box::new(None),
                email: None,
            },
            AuthenticationResponseData::AuthNResponse {
                authn_flow_type,
                authentication_value,
                trans_status,
                connector_metadata,
                ds_trans_id,
                eci,
            } => {
                authentication_value
                    .async_map(|auth_val| {
                        crate::core::payment_methods::vault::create_tokenize(
                            state,
                            auth_val.expose(),
                            None,
                            authentication
                                .authentication_id
                                .get_string_repr()
                                .to_string(),
                            merchant_key_store.key.get_inner(),
                        )
                    })
                    .await
                    .transpose()?;

                let authentication_status =
                    common_enums::AuthenticationStatus::foreign_from(trans_status.clone());

                storage::AuthenticationUpdate::AuthenticationUpdate {
                    trans_status,
                    acs_url: authn_flow_type.get_acs_url(),
                    challenge_request: authn_flow_type.get_challenge_request(),
                    acs_reference_number: authn_flow_type.get_acs_reference_number(),
                    acs_trans_id: authn_flow_type.get_acs_trans_id(),
                    acs_signed_content: authn_flow_type.get_acs_signed_content(),
                    authentication_type: authn_flow_type.get_decoupled_authentication_type(),
                    authentication_status,
                    connector_metadata,
                    ds_trans_id,
                    eci,
                }
            }
            AuthenticationResponseData::PostAuthNResponse {
                trans_status,
                authentication_value,
                eci,
            } => {
                authentication_value
                    .async_map(|auth_val| {
                        crate::core::payment_methods::vault::create_tokenize(
                            state,
                            auth_val.expose(),
                            None,
                            authentication
                                .authentication_id
                                .get_string_repr()
                                .to_string(),
                            merchant_key_store.key.get_inner(),
                        )
                    })
                    .await
                    .transpose()?;
                storage::AuthenticationUpdate::PostAuthenticationUpdate {
                    authentication_status: common_enums::AuthenticationStatus::foreign_from(
                        trans_status.clone(),
                    ),
                    trans_status,
                    eci,
                }
            }
            AuthenticationResponseData::PreAuthVersionCallResponse {
                maximum_supported_3ds_version,
            } => storage::AuthenticationUpdate::PreAuthenticationVersionCallUpdate {
                message_version: maximum_supported_3ds_version.clone(),
                maximum_supported_3ds_version,
            },
            AuthenticationResponseData::PreAuthThreeDsMethodCallResponse {
                threeds_server_transaction_id,
                three_ds_method_data,
                three_ds_method_url,
                connector_metadata,
            } => storage::AuthenticationUpdate::PreAuthenticationThreeDsMethodCall {
                threeds_server_transaction_id,
                three_ds_method_data,
                three_ds_method_url,
                connector_metadata,
                acquirer_bin: acquirer_details
                    .as_ref()
                    .map(|acquirer_details| acquirer_details.acquirer_bin.clone()),
                acquirer_merchant_id: acquirer_details
                    .map(|acquirer_details| acquirer_details.acquirer_merchant_id),
            },
        },
        Err(error) => storage::AuthenticationUpdate::ErrorUpdate {
            connector_authentication_id: error.connector_transaction_id,
            authentication_status: common_enums::AuthenticationStatus::Failed,
            error_message: error
                .reason
                .map(|reason| format!("message: {}, reason: {}", error.message, reason))
                .or(Some(error.message)),
            error_code: Some(error.code),
        },
    };
    state
        .store
        .update_authentication_by_merchant_id_authentication_id(
            authentication,
            authentication_update,
        )
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Error while updating authentication")
}

impl ForeignFrom<common_enums::AuthenticationStatus> for common_enums::AttemptStatus {
    fn foreign_from(from: common_enums::AuthenticationStatus) -> Self {
        match from {
            common_enums::AuthenticationStatus::Started
            | common_enums::AuthenticationStatus::Pending => Self::AuthenticationPending,
            common_enums::AuthenticationStatus::Success => Self::AuthenticationSuccessful,
            common_enums::AuthenticationStatus::Failed => Self::AuthenticationFailed,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn create_new_authentication(
    state: &SessionState,
    merchant_id: common_utils::id_type::MerchantId,
    authentication_connector: String,
    token: String,
    profile_id: common_utils::id_type::ProfileId,
    payment_id: common_utils::id_type::PaymentId,
    merchant_connector_id: common_utils::id_type::MerchantConnectorAccountId,
    organization_id: common_utils::id_type::OrganizationId,
    force_3ds_challenge: Option<bool>,
    psd2_sca_exemption_type: Option<common_enums::ScaExemptionType>,
) -> RouterResult<storage::Authentication> {
    let authentication_id = common_utils::id_type::AuthenticationId::generate_authentication_id(
        consts::AUTHENTICATION_ID_PREFIX,
    );
    let authentication_client_secret = Some(common_utils::generate_id_with_default_len(&format!(
        "{}_secret",
        authentication_id.get_string_repr()
    )));
    let new_authorization = storage::AuthenticationNew {
        authentication_id: authentication_id.clone(),
        merchant_id,
        authentication_connector: Some(authentication_connector),
        connector_authentication_id: None,
        payment_method_id: format!("eph_{token}"),
        authentication_type: None,
        authentication_status: common_enums::AuthenticationStatus::Started,
        authentication_lifecycle_status: common_enums::AuthenticationLifecycleStatus::Unused,
        error_message: None,
        error_code: None,
        connector_metadata: None,
        maximum_supported_version: None,
        threeds_server_transaction_id: None,
        cavv: None,
        authentication_flow_type: None,
        message_version: None,
        eci: None,
        trans_status: None,
        acquirer_bin: None,
        acquirer_merchant_id: None,
        three_ds_method_data: None,
        three_ds_method_url: None,
        acs_url: None,
        challenge_request: None,
        acs_reference_number: None,
        acs_trans_id: None,
        acs_signed_content: None,
        profile_id,
        payment_id: Some(payment_id),
        merchant_connector_id: Some(merchant_connector_id),
        ds_trans_id: None,
        directory_server_id: None,
        acquirer_country_code: None,
        service_details: None,
        organization_id,
        authentication_client_secret,
        force_3ds_challenge,
        psd2_sca_exemption_type,
        return_url: None,
        amount: None,
        currency: None,
        billing_address: None,
        shipping_address: None,
        browser_info: None,
        email: None,
        profile_acquirer_id: None,
    };
    state
        .store
        .insert_authentication(new_authorization)
        .await
        .to_duplicate_response(errors::ApiErrorResponse::GenericDuplicateError {
            message: format!(
                "Authentication with authentication_id {} already exists",
                authentication_id.get_string_repr()
            ),
        })
}

pub async fn do_auth_connector_call<F, Req, Res>(
    state: &SessionState,
    authentication_connector_name: String,
    router_data: RouterData<F, Req, Res>,
) -> RouterResult<RouterData<F, Req, Res>>
where
    Req: std::fmt::Debug + Clone + 'static,
    Res: std::fmt::Debug + Clone + 'static,
    F: std::fmt::Debug + Clone + 'static,
    dyn api::Connector + Sync: services::api::ConnectorIntegration<F, Req, Res>,
    dyn api::ConnectorV2 + Sync:
        services::api::ConnectorIntegrationV2<F, ExternalAuthenticationFlowData, Req, Res>,
{
    let connector_data =
        api::AuthenticationConnectorData::get_connector_by_name(&authentication_connector_name)?;
    let connector_integration: services::BoxedExternalAuthenticationConnectorIntegrationInterface<
        F,
        Req,
        Res,
    > = connector_data.connector.get_connector_integration();
    let router_data = execute_connector_processing_step(
        state,
        connector_integration,
        &router_data,
        payments::CallConnectorAction::Trigger,
        None,
        None,
    )
    .await
    .to_payment_failed_response()?;
    Ok(router_data)
}

pub async fn get_authentication_connector_data(
    state: &SessionState,
    key_store: &domain::MerchantKeyStore,
    business_profile: &domain::Profile,
    authentication_connector: Option<String>,
) -> RouterResult<(
    common_enums::AuthenticationConnectors,
    payments::helpers::MerchantConnectorAccountType,
)> {
    let authentication_connector = if let Some(authentication_connector) = authentication_connector
    {
        api_models::enums::convert_authentication_connector(&authentication_connector).ok_or(
            errors::ApiErrorResponse::UnprocessableEntity {
                message: format!(
                "Invalid authentication_connector found in request : {authentication_connector}",
            ),
            },
        )?
    } else {
        let authentication_details = business_profile
            .authentication_connector_details
            .clone()
            .get_required_value("authentication_details")
            .change_context(errors::ApiErrorResponse::UnprocessableEntity {
                message: "authentication_connector_details is not available in business profile"
                    .into(),
            })
            .attach_printable("authentication_connector_details not configured by the merchant")?;

        authentication_details
            .authentication_connectors
            .first()
            .ok_or(errors::ApiErrorResponse::UnprocessableEntity {
                message: format!(
                    "No authentication_connector found for profile_id {:?}",
                    business_profile.get_id()
                ),
            })
            .attach_printable(
                "No authentication_connector found from merchant_account.authentication_details",
            )?
            .to_owned()
    };

    let profile_id = business_profile.get_id();
    let authentication_connector_mca = payments::helpers::get_merchant_connector_account(
        state,
        &business_profile.merchant_id,
        None,
        key_store,
        profile_id,
        authentication_connector.to_string().as_str(),
        None,
    )
    .await?;
    Ok((authentication_connector, authentication_connector_mca))
}
