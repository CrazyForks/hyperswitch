pub mod client;
pub mod generic_link_response;
pub mod request;
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    future::Future,
    str,
    sync::Arc,
    time::{Duration, Instant},
};

use actix_http::header::HeaderMap;
use actix_web::{
    body,
    http::header::{HeaderName, HeaderValue},
    web, FromRequest, HttpRequest, HttpResponse, Responder, ResponseError,
};
pub use client::{ApiClient, MockApiClient, ProxyClient};
pub use common_enums::enums::PaymentAction;
pub use common_utils::request::{ContentType, Method, Request, RequestBuilder};
use common_utils::{
    consts::{DEFAULT_TENANT, TENANT_HEADER, X_HS_LATENCY},
    errors::{ErrorSwitch, ReportSwitchExt},
    request::RequestContent,
};
use error_stack::{report, Report, ResultExt};
use hyperswitch_domain_models::router_data_v2::flow_common_types as common_types;
pub use hyperswitch_domain_models::{
    api::{
        ApplicationResponse, GenericExpiredLinkData, GenericLinkFormData, GenericLinkStatusData,
        GenericLinks, PaymentLinkAction, PaymentLinkFormData, PaymentLinkStatusData,
        RedirectionFormData,
    },
    payment_method_data::PaymentMethodData,
    router_response_types::RedirectForm,
};
pub use hyperswitch_interfaces::{
    api::{
        BoxedConnectorIntegration, CaptureSyncMethod, ConnectorIntegration,
        ConnectorIntegrationAny, ConnectorRedirectResponse, ConnectorSpecifications,
        ConnectorValidation,
    },
    connector_integration_v2::{
        BoxedConnectorIntegrationV2, ConnectorIntegrationAnyV2, ConnectorIntegrationV2,
    },
};
use masking::{Maskable, PeekInterface, Secret};
use router_env::{instrument, tracing, tracing_actix_web::RequestId, Tag};
use serde::Serialize;
use serde_json::json;
use tera::{Context, Error as TeraError, Tera};

use super::{
    authentication::AuthenticateAndFetch,
    connector_integration_interface::BoxedConnectorIntegrationInterface,
};
use crate::{
    configs::Settings,
    consts,
    core::{
        api_locking,
        errors::{self, CustomResult},
        payments,
    },
    events::{
        api_logs::{ApiEvent, ApiEventMetric, ApiEventsType},
        connector_api_logs::ConnectorEvent,
    },
    headers, logger,
    routes::{
        app::{AppStateInfo, ReqState, SessionStateInfo},
        metrics, AppState, SessionState,
    },
    services::{
        connector_integration_interface::RouterDataConversion,
        generic_link_response::build_generic_link_html,
    },
    types::{self, api, ErrorResponse},
    utils,
};

pub type BoxedPaymentConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::PaymentFlowData, Req, Resp>;
pub type BoxedRefundConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::RefundFlowData, Req, Resp>;
#[cfg(feature = "frm")]
pub type BoxedFrmConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::FrmFlowData, Req, Resp>;
pub type BoxedDisputeConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::DisputesFlowData, Req, Resp>;
pub type BoxedMandateRevokeConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::MandateRevokeFlowData, Req, Resp>;
#[cfg(feature = "payouts")]
pub type BoxedPayoutConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::PayoutFlowData, Req, Resp>;
pub type BoxedWebhookSourceVerificationConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::WebhookSourceVerifyData, Req, Resp>;
pub type BoxedExternalAuthenticationConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::ExternalAuthenticationFlowData, Req, Resp>;
pub type BoxedAccessTokenConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::AccessTokenFlowData, Req, Resp>;
pub type BoxedFilesConnectorIntegrationInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::FilesFlowData, Req, Resp>;
pub type BoxedRevenueRecoveryRecordBackInterface<T, Req, Res> =
    BoxedConnectorIntegrationInterface<T, common_types::RevenueRecoveryRecordBackData, Req, Res>;
pub type BoxedBillingConnectorInvoiceSyncIntegrationInterface<T, Req, Res> =
    BoxedConnectorIntegrationInterface<
        T,
        common_types::BillingConnectorInvoiceSyncFlowData,
        Req,
        Res,
    >;

pub type BoxedUnifiedAuthenticationServiceInterface<T, Req, Resp> =
    BoxedConnectorIntegrationInterface<T, common_types::UasFlowData, Req, Resp>;

pub type BoxedBillingConnectorPaymentsSyncIntegrationInterface<T, Req, Res> =
    BoxedConnectorIntegrationInterface<
        T,
        common_types::BillingConnectorPaymentsSyncFlowData,
        Req,
        Res,
    >;
pub type BoxedVaultConnectorIntegrationInterface<T, Req, Res> =
    BoxedConnectorIntegrationInterface<T, common_types::VaultConnectorFlowData, Req, Res>;

/// Handle the flow by interacting with connector module
/// `connector_request` is applicable only in case if the `CallConnectorAction` is `Trigger`
/// In other cases, It will be created if required, even if it is not passed
#[instrument(skip_all, fields(connector_name, payment_method))]
pub async fn execute_connector_processing_step<
    'b,
    'a,
    T,
    ResourceCommonData: Clone + RouterDataConversion<T, Req, Resp> + 'static,
    Req: Debug + Clone + 'static,
    Resp: Debug + Clone + 'static,
>(
    state: &'b SessionState,
    connector_integration: BoxedConnectorIntegrationInterface<T, ResourceCommonData, Req, Resp>,
    req: &'b types::RouterData<T, Req, Resp>,
    call_connector_action: payments::CallConnectorAction,
    connector_request: Option<Request>,
    return_raw_connector_response: Option<bool>,
) -> CustomResult<types::RouterData<T, Req, Resp>, errors::ConnectorError>
where
    T: Clone + Debug + 'static,
    // BoxedConnectorIntegration<T, Req, Resp>: 'b,
{
    // If needed add an error stack as follows
    // connector_integration.build_request(req).attach_printable("Failed to build request");
    tracing::Span::current().record("connector_name", &req.connector);
    tracing::Span::current().record("payment_method", req.payment_method.to_string());
    logger::debug!(connector_request=?connector_request);
    let mut router_data = req.clone();
    match call_connector_action {
        payments::CallConnectorAction::HandleResponse(res) => {
            let response = types::Response {
                headers: None,
                response: res.into(),
                status_code: 200,
            };
            connector_integration.handle_response(req, None, response)
        }
        payments::CallConnectorAction::Avoid => Ok(router_data),
        payments::CallConnectorAction::StatusUpdate {
            status,
            error_code,
            error_message,
        } => {
            router_data.status = status;
            let error_response = if error_code.is_some() | error_message.is_some() {
                Some(ErrorResponse {
                    code: error_code.unwrap_or(consts::NO_ERROR_CODE.to_string()),
                    message: error_message.unwrap_or(consts::NO_ERROR_MESSAGE.to_string()),
                    status_code: 200, // This status code is ignored in redirection response it will override with 302 status code.
                    reason: None,
                    attempt_status: None,
                    connector_transaction_id: None,
                    network_advice_code: None,
                    network_decline_code: None,
                    network_error_message: None,
                })
            } else {
                None
            };
            router_data.response = error_response.map(Err).unwrap_or(router_data.response);
            Ok(router_data)
        }
        payments::CallConnectorAction::Trigger => {
            metrics::CONNECTOR_CALL_COUNT.add(
                1,
                router_env::metric_attributes!(
                    ("connector", req.connector.to_string()),
                    (
                        "flow",
                        std::any::type_name::<T>()
                            .split("::")
                            .last()
                            .unwrap_or_default()
                    ),
                ),
            );

            let connector_request = match connector_request {
                Some(connector_request) => Some(connector_request),
                None => connector_integration
                    .build_request(req, &state.conf.connectors)
                    .inspect_err(|error| {
                        if matches!(
                            error.current_context(),
                            &errors::ConnectorError::RequestEncodingFailed
                                | &errors::ConnectorError::RequestEncodingFailedWithReason(_)
                        ) {
                            metrics::REQUEST_BUILD_FAILURE.add(
                                1,
                                router_env::metric_attributes!((
                                    "connector",
                                    req.connector.clone()
                                )),
                            )
                        }
                    })?,
            };

            match connector_request {
                Some(request) => {
                    let masked_request_body = match &request.body {
                        Some(request) => match request {
                            RequestContent::Json(i)
                            | RequestContent::FormUrlEncoded(i)
                            | RequestContent::Xml(i) => i
                                .masked_serialize()
                                .unwrap_or(json!({ "error": "failed to mask serialize"})),
                            RequestContent::FormData(_) => json!({"request_type": "FORM_DATA"}),
                            RequestContent::RawBytes(_) => json!({"request_type": "RAW_BYTES"}),
                        },
                        None => serde_json::Value::Null,
                    };
                    let request_url = request.url.clone();
                    let request_method = request.method;
                    let current_time = Instant::now();
                    let response =
                        call_connector_api(state, request, "execute_connector_processing_step")
                            .await;
                    let external_latency = current_time.elapsed().as_millis();
                    logger::info!(raw_connector_request=?masked_request_body);
                    let status_code = response
                        .as_ref()
                        .map(|i| {
                            i.as_ref()
                                .map_or_else(|value| value.status_code, |value| value.status_code)
                        })
                        .unwrap_or_default();
                    let mut connector_event = ConnectorEvent::new(
                        state.tenant.tenant_id.clone(),
                        req.connector.clone(),
                        std::any::type_name::<T>(),
                        masked_request_body,
                        request_url,
                        request_method,
                        req.payment_id.clone(),
                        req.merchant_id.clone(),
                        state.request_id.as_ref(),
                        external_latency,
                        req.refund_id.clone(),
                        req.dispute_id.clone(),
                        status_code,
                    );

                    match response {
                        Ok(body) => {
                            let response = match body {
                                Ok(body) => {
                                    let connector_http_status_code = Some(body.status_code);
                                    let handle_response_result = connector_integration
                                        .handle_response(req, Some(&mut connector_event), body.clone())
                                        .inspect_err(|error| {
                                            if error.current_context()
                                            == &errors::ConnectorError::ResponseDeserializationFailed
                                        {
                                            metrics::RESPONSE_DESERIALIZATION_FAILURE.add(

                                                1,
                                                router_env::metric_attributes!((
                                                    "connector",
                                                    req.connector.clone(),
                                                )),
                                            )
                                        }
                                        });
                                    match handle_response_result {
                                        Ok(mut data) => {
                                            state.event_handler().log_event(&connector_event);
                                            data.connector_http_status_code =
                                                connector_http_status_code;
                                            // Add up multiple external latencies in case of multiple external calls within the same request.
                                            data.external_latency = Some(
                                                data.external_latency
                                                    .map_or(external_latency, |val| {
                                                        val + external_latency
                                                    }),
                                            );
                                            if return_raw_connector_response == Some(true) {
                                                let mut decoded = String::from_utf8(body.response.as_ref().to_vec())
                                                    .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
                                                if decoded.starts_with('\u{feff}') {
                                                    decoded = decoded
                                                        .trim_start_matches('\u{feff}')
                                                        .to_string();
                                                }
                                                data.raw_connector_response =
                                                    Some(Secret::new(decoded));
                                            }
                                            Ok(data)
                                        }
                                        Err(err) => {
                                            connector_event
                                                .set_error(json!({"error": err.to_string()}));

                                            state.event_handler().log_event(&connector_event);
                                            Err(err)
                                        }
                                    }?
                                }
                                Err(body) => {
                                    router_data.connector_http_status_code = Some(body.status_code);
                                    router_data.external_latency = Some(
                                        router_data
                                            .external_latency
                                            .map_or(external_latency, |val| val + external_latency),
                                    );
                                    metrics::CONNECTOR_ERROR_RESPONSE_COUNT.add(
                                        1,
                                        router_env::metric_attributes!((
                                            "connector",
                                            req.connector.clone(),
                                        )),
                                    );

                                    let error = match body.status_code {
                                        500..=511 => {
                                            let error_res = connector_integration
                                                .get_5xx_error_response(
                                                    body,
                                                    Some(&mut connector_event),
                                                )?;
                                            state.event_handler().log_event(&connector_event);
                                            error_res
                                        }
                                        _ => {
                                            let error_res = connector_integration
                                                .get_error_response(
                                                    body,
                                                    Some(&mut connector_event),
                                                )?;
                                            if let Some(status) = error_res.attempt_status {
                                                router_data.status = status;
                                            };
                                            state.event_handler().log_event(&connector_event);
                                            error_res
                                        }
                                    };

                                    router_data.response = Err(error);

                                    router_data
                                }
                            };
                            Ok(response)
                        }
                        Err(error) => {
                            connector_event.set_error(json!({"error": error.to_string()}));
                            state.event_handler().log_event(&connector_event);
                            if error.current_context().is_upstream_timeout() {
                                let error_response = ErrorResponse {
                                    code: consts::REQUEST_TIMEOUT_ERROR_CODE.to_string(),
                                    message: consts::REQUEST_TIMEOUT_ERROR_MESSAGE.to_string(),
                                    reason: Some(consts::REQUEST_TIMEOUT_ERROR_MESSAGE.to_string()),
                                    status_code: 504,
                                    attempt_status: None,
                                    connector_transaction_id: None,
                                    network_advice_code: None,
                                    network_decline_code: None,
                                    network_error_message: None,
                                };
                                router_data.response = Err(error_response);
                                router_data.connector_http_status_code = Some(504);
                                router_data.external_latency = Some(
                                    router_data
                                        .external_latency
                                        .map_or(external_latency, |val| val + external_latency),
                                );
                                Ok(router_data)
                            } else {
                                Err(error.change_context(
                                    errors::ConnectorError::ProcessingStepFailed(None),
                                ))
                            }
                        }
                    }
                }
                None => Ok(router_data),
            }
        }
    }
}

#[instrument(skip_all)]
pub async fn call_connector_api(
    state: &SessionState,
    request: Request,
    flow_name: &str,
) -> CustomResult<Result<types::Response, types::Response>, errors::ApiClientError> {
    let current_time = Instant::now();
    let headers = request.headers.clone();
    let url = request.url.clone();
    let response = state
        .api_client
        .send_request(state, request, None, true)
        .await;

    match response.as_ref() {
        Ok(resp) => {
            let status_code = resp.status().as_u16();
            let elapsed_time = current_time.elapsed();
            logger::info!(
                ?headers,
                url,
                status_code,
                flow=?flow_name,
                ?elapsed_time
            );
        }
        Err(err) => {
            logger::info!(
                call_connector_api_error=?err
            );
        }
    }

    handle_response(response).await
}

#[instrument(skip_all)]
async fn handle_response(
    response: CustomResult<reqwest::Response, errors::ApiClientError>,
) -> CustomResult<Result<types::Response, types::Response>, errors::ApiClientError> {
    response
        .map(|response| async {
            logger::info!(?response);
            let status_code = response.status().as_u16();
            let headers = Some(response.headers().to_owned());

            match status_code {
                200..=202 | 302 | 204 => {
                    // If needed add log line
                    // logger:: error!( error_parsing_response=?err);
                    let response = response
                        .bytes()
                        .await
                        .change_context(errors::ApiClientError::ResponseDecodingFailed)
                        .attach_printable("Error while waiting for response")?;
                    Ok(Ok(types::Response {
                        headers,
                        response,
                        status_code,
                    }))
                }

                status_code @ 500..=599 => {
                    let bytes = response.bytes().await.map_err(|error| {
                        report!(error)
                            .change_context(errors::ApiClientError::ResponseDecodingFailed)
                            .attach_printable("Client error response received")
                    })?;
                    // let error = match status_code {
                    //     500 => errors::ApiClientError::InternalServerErrorReceived,
                    //     502 => errors::ApiClientError::BadGatewayReceived,
                    //     503 => errors::ApiClientError::ServiceUnavailableReceived,
                    //     504 => errors::ApiClientError::GatewayTimeoutReceived,
                    //     _ => errors::ApiClientError::UnexpectedServerResponse,
                    // };
                    Ok(Err(types::Response {
                        headers,
                        response: bytes,
                        status_code,
                    }))
                }

                status_code @ 400..=499 => {
                    let bytes = response.bytes().await.map_err(|error| {
                        report!(error)
                            .change_context(errors::ApiClientError::ResponseDecodingFailed)
                            .attach_printable("Client error response received")
                    })?;
                    /* let error = match status_code {
                        400 => errors::ApiClientError::BadRequestReceived(bytes),
                        401 => errors::ApiClientError::UnauthorizedReceived(bytes),
                        403 => errors::ApiClientError::ForbiddenReceived,
                        404 => errors::ApiClientError::NotFoundReceived(bytes),
                        405 => errors::ApiClientError::MethodNotAllowedReceived,
                        408 => errors::ApiClientError::RequestTimeoutReceived,
                        422 => errors::ApiClientError::UnprocessableEntityReceived(bytes),
                        429 => errors::ApiClientError::TooManyRequestsReceived,
                        _ => errors::ApiClientError::UnexpectedServerResponse,
                    };
                    Err(report!(error).attach_printable("Client error response received"))
                        */
                    Ok(Err(types::Response {
                        headers,
                        response: bytes,
                        status_code,
                    }))
                }

                _ => Err(report!(errors::ApiClientError::UnexpectedServerResponse)
                    .attach_printable("Unexpected response from server")),
            }
        })?
        .await
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct ApplicationRedirectResponse {
    pub url: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AuthFlow {
    Client,
    Merchant,
}

#[allow(clippy::too_many_arguments)]
#[instrument(
    skip(request, payload, state, func, api_auth, incoming_request_header),
    fields(merchant_id)
)]
pub async fn server_wrap_util<'a, 'b, U, T, Q, F, Fut, E, OErr>(
    flow: &'a impl router_env::types::FlowMetric,
    state: web::Data<AppState>,
    incoming_request_header: &HeaderMap,
    request: &'a HttpRequest,
    payload: T,
    func: F,
    api_auth: &dyn AuthenticateAndFetch<U, SessionState>,
    lock_action: api_locking::LockAction,
) -> CustomResult<ApplicationResponse<Q>, OErr>
where
    F: Fn(SessionState, U, T, ReqState) -> Fut,
    'b: 'a,
    Fut: Future<Output = CustomResult<ApplicationResponse<Q>, E>>,
    Q: Serialize + Debug + 'a + ApiEventMetric,
    T: Debug + Serialize + ApiEventMetric,
    E: ErrorSwitch<OErr> + error_stack::Context,
    OErr: ResponseError + error_stack::Context + Serialize,
    errors::ApiErrorResponse: ErrorSwitch<OErr>,
{
    let request_id = RequestId::extract(request)
        .await
        .attach_printable("Unable to extract request id from request")
        .change_context(errors::ApiErrorResponse::InternalServerError.switch())?;

    let mut app_state = state.get_ref().clone();

    let start_instant = Instant::now();
    let serialized_request = masking::masked_serialize(&payload)
        .attach_printable("Failed to serialize json request")
        .change_context(errors::ApiErrorResponse::InternalServerError.switch())?;

    let mut event_type = payload.get_api_event_type();
    let tenant_id = if !state.conf.multitenancy.enabled {
        common_utils::id_type::TenantId::try_from_string(DEFAULT_TENANT.to_owned())
            .attach_printable("Unable to get default tenant id")
            .change_context(errors::ApiErrorResponse::InternalServerError.switch())?
    } else {
        let request_tenant_id = incoming_request_header
            .get(TENANT_HEADER)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| errors::ApiErrorResponse::MissingTenantId.switch())
            .and_then(|header_value| {
                common_utils::id_type::TenantId::try_from_string(header_value.to_string()).map_err(
                    |_| {
                        errors::ApiErrorResponse::InvalidRequestData {
                            message: format!("`{}` header is invalid", headers::X_TENANT_ID),
                        }
                        .switch()
                    },
                )
            })?;

        state
            .conf
            .multitenancy
            .get_tenant(&request_tenant_id)
            .map(|tenant| tenant.tenant_id.clone())
            .ok_or(
                errors::ApiErrorResponse::InvalidTenant {
                    tenant_id: request_tenant_id.get_string_repr().to_string(),
                }
                .switch(),
            )?
    };
    let locale = utils::get_locale_from_header(&incoming_request_header.clone());
    let mut session_state =
        Arc::new(app_state.clone()).get_session_state(&tenant_id, Some(locale), || {
            errors::ApiErrorResponse::InvalidTenant {
                tenant_id: tenant_id.get_string_repr().to_string(),
            }
            .switch()
        })?;
    session_state.add_request_id(request_id);
    let mut request_state = session_state.get_req_state();

    request_state.event_context.record_info(request_id);
    request_state
        .event_context
        .record_info(("flow".to_string(), flow.to_string()));

    request_state.event_context.record_info((
        "tenant_id".to_string(),
        tenant_id.get_string_repr().to_string(),
    ));

    // Currently auth failures are not recorded as API events
    let (auth_out, auth_type) = api_auth
        .authenticate_and_fetch(request.headers(), &session_state)
        .await
        .switch()?;

    request_state.event_context.record_info(auth_type.clone());

    let merchant_id = auth_type
        .get_merchant_id()
        .cloned()
        .unwrap_or(common_utils::id_type::MerchantId::get_merchant_id_not_found());

    app_state.add_flow_name(flow.to_string());

    tracing::Span::current().record("merchant_id", merchant_id.get_string_repr().to_owned());

    let output = {
        lock_action
            .clone()
            .perform_locking_action(&session_state, merchant_id.to_owned())
            .await
            .switch()?;
        let res = func(session_state.clone(), auth_out, payload, request_state)
            .await
            .switch();
        lock_action
            .free_lock_action(&session_state, merchant_id.to_owned())
            .await
            .switch()?;
        res
    };
    let request_duration = Instant::now()
        .saturating_duration_since(start_instant)
        .as_millis();

    let mut serialized_response = None;
    let mut error = None;
    let mut overhead_latency = None;

    let status_code = match output.as_ref() {
        Ok(res) => {
            if let ApplicationResponse::Json(data) = res {
                serialized_response.replace(
                    masking::masked_serialize(&data)
                        .attach_printable("Failed to serialize json response")
                        .change_context(errors::ApiErrorResponse::InternalServerError.switch())?,
                );
            } else if let ApplicationResponse::JsonWithHeaders((data, headers)) = res {
                serialized_response.replace(
                    masking::masked_serialize(&data)
                        .attach_printable("Failed to serialize json response")
                        .change_context(errors::ApiErrorResponse::InternalServerError.switch())?,
                );

                if let Some((_, value)) = headers.iter().find(|(key, _)| key == X_HS_LATENCY) {
                    if let Ok(external_latency) = value.clone().into_inner().parse::<u128>() {
                        overhead_latency.replace(external_latency);
                    }
                }
            }
            event_type = res.get_api_event_type().or(event_type);

            metrics::request::track_response_status_code(res)
        }
        Err(err) => {
            error.replace(
                serde_json::to_value(err.current_context())
                    .attach_printable("Failed to serialize json response")
                    .change_context(errors::ApiErrorResponse::InternalServerError.switch())
                    .ok()
                    .into(),
            );
            err.current_context().status_code().as_u16().into()
        }
    };

    let infra = extract_mapped_fields(
        &serialized_request,
        state.enhancement.as_ref(),
        state.infra_components.as_ref(),
    );

    let api_event = ApiEvent::new(
        tenant_id,
        Some(merchant_id.clone()),
        flow,
        &request_id,
        request_duration,
        status_code,
        serialized_request,
        serialized_response,
        overhead_latency,
        auth_type,
        error,
        event_type.unwrap_or(ApiEventsType::Miscellaneous),
        request,
        request.method(),
        infra.clone(),
    );

    state.event_handler().log_event(&api_event);

    output
}

#[instrument(
    skip(request, state, func, api_auth, payload),
    fields(request_method, request_url_path, status_code)
)]
pub async fn server_wrap<'a, T, U, Q, F, Fut, E>(
    flow: impl router_env::types::FlowMetric,
    state: web::Data<AppState>,
    request: &'a HttpRequest,
    payload: T,
    func: F,
    api_auth: &dyn AuthenticateAndFetch<U, SessionState>,
    lock_action: api_locking::LockAction,
) -> HttpResponse
where
    F: Fn(SessionState, U, T, ReqState) -> Fut,
    Fut: Future<Output = CustomResult<ApplicationResponse<Q>, E>>,
    Q: Serialize + Debug + ApiEventMetric + 'a,
    T: Debug + Serialize + ApiEventMetric,
    ApplicationResponse<Q>: Debug,
    E: ErrorSwitch<api_models::errors::types::ApiErrorResponse> + error_stack::Context,
{
    let request_method = request.method().as_str();
    let url_path = request.path();

    let unmasked_incoming_header_keys = state.conf().unmasked_headers.keys;

    let incoming_request_header = request.headers();

    let incoming_header_to_log: HashMap<String, HeaderValue> =
        incoming_request_header
            .iter()
            .fold(HashMap::new(), |mut acc, (key, value)| {
                let key = key.to_string();
                if unmasked_incoming_header_keys.contains(&key.as_str().to_lowercase()) {
                    acc.insert(key.clone(), value.clone());
                } else {
                    acc.insert(key.clone(), HeaderValue::from_static("**MASKED**"));
                }
                acc
            });

    tracing::Span::current().record("request_method", request_method);
    tracing::Span::current().record("request_url_path", url_path);

    let start_instant = Instant::now();

    logger::info!(
        tag = ?Tag::BeginRequest, payload = ?payload,
    headers = ?incoming_header_to_log);

    let server_wrap_util_res = server_wrap_util(
        &flow,
        state.clone(),
        incoming_request_header,
        request,
        payload,
        func,
        api_auth,
        lock_action,
    )
    .await
    .map(|response| {
        logger::info!(api_response =? response);
        response
    });

    let res = match server_wrap_util_res {
        Ok(ApplicationResponse::Json(response)) => match serde_json::to_string(&response) {
            Ok(res) => http_response_json(res),
            Err(_) => http_response_err(
                r#"{
                    "error": {
                        "message": "Error serializing response from connector"
                    }
                }"#,
            ),
        },
        Ok(ApplicationResponse::StatusOk) => http_response_ok(),
        Ok(ApplicationResponse::TextPlain(text)) => http_response_plaintext(text),
        Ok(ApplicationResponse::FileData((file_data, content_type))) => {
            http_response_file_data(file_data, content_type)
        }
        Ok(ApplicationResponse::JsonForRedirection(response)) => {
            match serde_json::to_string(&response) {
                Ok(res) => http_redirect_response(res, response),
                Err(_) => http_response_err(
                    r#"{
                    "error": {
                        "message": "Error serializing response from connector"
                    }
                }"#,
                ),
            }
        }
        Ok(ApplicationResponse::Form(redirection_data)) => {
            let config = state.conf();
            build_redirection_form(
                &redirection_data.redirect_form,
                redirection_data.payment_method_data,
                redirection_data.amount,
                redirection_data.currency,
                config,
            )
            .respond_to(request)
            .map_into_boxed_body()
        }

        Ok(ApplicationResponse::GenericLinkForm(boxed_generic_link_data)) => {
            let link_type = boxed_generic_link_data.data.to_string();
            match build_generic_link_html(
                boxed_generic_link_data.data,
                boxed_generic_link_data.locale,
            ) {
                Ok(rendered_html) => {
                    let headers = if !boxed_generic_link_data.allowed_domains.is_empty() {
                        let domains_str = boxed_generic_link_data
                            .allowed_domains
                            .into_iter()
                            .collect::<Vec<String>>()
                            .join(" ");
                        let csp_header = format!("frame-ancestors 'self' {domains_str};");
                        Some(HashSet::from([("content-security-policy", csp_header)]))
                    } else {
                        None
                    };
                    http_response_html_data(rendered_html, headers)
                }
                Err(_) => http_response_err(format!("Error while rendering {link_type} HTML page")),
            }
        }

        Ok(ApplicationResponse::PaymentLinkForm(boxed_payment_link_data)) => {
            match *boxed_payment_link_data {
                PaymentLinkAction::PaymentLinkFormData(payment_link_data) => {
                    match build_payment_link_html(payment_link_data) {
                        Ok(rendered_html) => http_response_html_data(rendered_html, None),
                        Err(_) => http_response_err(
                            r#"{
                                "error": {
                                    "message": "Error while rendering payment link html page"
                                }
                            }"#,
                        ),
                    }
                }
                PaymentLinkAction::PaymentLinkStatus(payment_link_data) => {
                    match get_payment_link_status(payment_link_data) {
                        Ok(rendered_html) => http_response_html_data(rendered_html, None),
                        Err(_) => http_response_err(
                            r#"{
                                "error": {
                                    "message": "Error while rendering payment link status page"
                                }
                            }"#,
                        ),
                    }
                }
            }
        }

        Ok(ApplicationResponse::JsonWithHeaders((response, headers))) => {
            let request_elapsed_time = request.headers().get(X_HS_LATENCY).and_then(|value| {
                if value == "true" {
                    Some(start_instant.elapsed())
                } else {
                    None
                }
            });
            match serde_json::to_string(&response) {
                Ok(res) => http_response_json_with_headers(res, headers, request_elapsed_time),
                Err(_) => http_response_err(
                    r#"{
                        "error": {
                            "message": "Error serializing response from connector"
                        }
                    }"#,
                ),
            }
        }
        Err(error) => log_and_return_error_response(error),
    };

    let response_code = res.status().as_u16();
    tracing::Span::current().record("status_code", response_code);

    let end_instant = Instant::now();
    let request_duration = end_instant.saturating_duration_since(start_instant);
    logger::info!(
        tag = ?Tag::EndRequest,
        time_taken_ms = request_duration.as_millis(),
    );
    res
}

pub fn log_and_return_error_response<T>(error: Report<T>) -> HttpResponse
where
    T: error_stack::Context + Clone + ResponseError,
    Report<T>: EmbedError,
{
    logger::error!(?error);
    HttpResponse::from_error(error.embed().current_context().clone())
}

pub trait EmbedError: Sized {
    fn embed(self) -> Self {
        self
    }
}

impl EmbedError for Report<api_models::errors::types::ApiErrorResponse> {
    fn embed(self) -> Self {
        #[cfg(feature = "detailed_errors")]
        {
            let mut report = self;
            let error_trace = serde_json::to_value(&report).ok().and_then(|inner| {
                serde_json::from_value::<Vec<errors::NestedErrorStack<'_>>>(inner)
                    .ok()
                    .map(Into::<errors::VecLinearErrorStack<'_>>::into)
                    .map(serde_json::to_value)
                    .transpose()
                    .ok()
                    .flatten()
            });

            match report.downcast_mut::<api_models::errors::types::ApiErrorResponse>() {
                None => {}
                Some(inner) => {
                    inner.get_internal_error_mut().stacktrace = error_trace;
                }
            }
            report
        }

        #[cfg(not(feature = "detailed_errors"))]
        self
    }
}

impl EmbedError
    for Report<hyperswitch_domain_models::errors::api_error_response::ApiErrorResponse>
{
}

pub fn http_response_json<T: body::MessageBody + 'static>(response: T) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(mime::APPLICATION_JSON)
        .body(response)
}

pub fn http_server_error_json_response<T: body::MessageBody + 'static>(
    response: T,
) -> HttpResponse {
    HttpResponse::InternalServerError()
        .content_type(mime::APPLICATION_JSON)
        .body(response)
}

pub fn http_response_json_with_headers<T: body::MessageBody + 'static>(
    response: T,
    headers: Vec<(String, Maskable<String>)>,
    request_duration: Option<Duration>,
) -> HttpResponse {
    let mut response_builder = HttpResponse::Ok();
    for (header_name, header_value) in headers {
        let is_sensitive_header = header_value.is_masked();
        let mut header_value = header_value.into_inner();
        if header_name == X_HS_LATENCY {
            if let Some(request_duration) = request_duration {
                if let Ok(external_latency) = header_value.parse::<u128>() {
                    let updated_duration = request_duration.as_millis() - external_latency;
                    header_value = updated_duration.to_string();
                }
            }
        }
        let mut header_value = match HeaderValue::from_str(header_value.as_str()) {
            Ok(header_value) => header_value,
            Err(error) => {
                logger::error!(?error);
                return http_server_error_json_response("Something Went Wrong");
            }
        };

        if is_sensitive_header {
            header_value.set_sensitive(true);
        }
        response_builder.append_header((header_name, header_value));
    }

    response_builder
        .content_type(mime::APPLICATION_JSON)
        .body(response)
}

pub fn http_response_plaintext<T: body::MessageBody + 'static>(res: T) -> HttpResponse {
    HttpResponse::Ok().content_type(mime::TEXT_PLAIN).body(res)
}

pub fn http_response_file_data<T: body::MessageBody + 'static>(
    res: T,
    content_type: mime::Mime,
) -> HttpResponse {
    HttpResponse::Ok().content_type(content_type).body(res)
}

pub fn http_response_html_data<T: body::MessageBody + 'static>(
    res: T,
    optional_headers: Option<HashSet<(&'static str, String)>>,
) -> HttpResponse {
    let mut res_builder = HttpResponse::Ok();
    res_builder.content_type(mime::TEXT_HTML);

    if let Some(headers) = optional_headers {
        for (key, value) in headers {
            if let Ok(header_val) = HeaderValue::try_from(value) {
                res_builder.insert_header((HeaderName::from_static(key), header_val));
            }
        }
    }

    res_builder.body(res)
}

pub fn http_response_ok() -> HttpResponse {
    HttpResponse::Ok().finish()
}

pub fn http_redirect_response<T: body::MessageBody + 'static>(
    response: T,
    redirection_response: api::RedirectionResponse,
) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(mime::APPLICATION_JSON)
        .append_header((
            "Location",
            redirection_response.return_url_with_query_params,
        ))
        .status(http::StatusCode::FOUND)
        .body(response)
}

pub fn http_response_err<T: body::MessageBody + 'static>(response: T) -> HttpResponse {
    HttpResponse::BadRequest()
        .content_type(mime::APPLICATION_JSON)
        .body(response)
}

pub trait Authenticate {
    fn get_client_secret(&self) -> Option<&String> {
        None
    }

    fn should_return_raw_response(&self) -> Option<bool> {
        None
    }
}

#[cfg(feature = "v2")]
impl Authenticate for api_models::payments::PaymentsConfirmIntentRequest {
    fn should_return_raw_response(&self) -> Option<bool> {
        self.return_raw_connector_response
    }
}
#[cfg(feature = "v2")]
impl Authenticate for api_models::payments::ProxyPaymentsRequest {}

#[cfg(feature = "v1")]
impl Authenticate for api_models::payments::PaymentsRequest {
    fn get_client_secret(&self) -> Option<&String> {
        self.client_secret.as_ref()
    }

    fn should_return_raw_response(&self) -> Option<bool> {
        // In v1, this maps to `all_keys_required` to retain backward compatibility.
        // The equivalent field in v2 is `return_raw_connector_response`.
        self.all_keys_required
    }
}

impl Authenticate for api_models::payment_methods::PaymentMethodListRequest {
    fn get_client_secret(&self) -> Option<&String> {
        self.client_secret.as_ref()
    }
}

#[cfg(feature = "v1")]
impl Authenticate for api_models::payments::PaymentsSessionRequest {
    fn get_client_secret(&self) -> Option<&String> {
        Some(&self.client_secret)
    }
}
impl Authenticate for api_models::payments::PaymentsDynamicTaxCalculationRequest {
    fn get_client_secret(&self) -> Option<&String> {
        Some(self.client_secret.peek())
    }
}

impl Authenticate for api_models::payments::PaymentsPostSessionTokensRequest {
    fn get_client_secret(&self) -> Option<&String> {
        Some(self.client_secret.peek())
    }
}

impl Authenticate for api_models::payments::PaymentsUpdateMetadataRequest {}
impl Authenticate for api_models::payments::PaymentsRetrieveRequest {
    #[cfg(feature = "v2")]
    fn should_return_raw_response(&self) -> Option<bool> {
        self.return_raw_connector_response
    }

    #[cfg(feature = "v1")]
    fn should_return_raw_response(&self) -> Option<bool> {
        // In v1, this maps to `all_keys_required` to retain backward compatibility.
        // The equivalent field in v2 is `return_raw_connector_response`.
        self.all_keys_required
    }
}
impl Authenticate for api_models::payments::PaymentsCancelRequest {}
impl Authenticate for api_models::payments::PaymentsCaptureRequest {}
impl Authenticate for api_models::payments::PaymentsIncrementalAuthorizationRequest {}
impl Authenticate for api_models::payments::PaymentsStartRequest {}
// impl Authenticate for api_models::payments::PaymentsApproveRequest {}
impl Authenticate for api_models::payments::PaymentsRejectRequest {}
// #[cfg(feature = "v2")]
// impl Authenticate for api_models::payments::PaymentsIntentResponse {}

pub fn build_redirection_form(
    form: &RedirectForm,
    payment_method_data: Option<PaymentMethodData>,
    amount: String,
    currency: String,
    config: Settings,
) -> maud::Markup {
    use maud::PreEscaped;
    let logging_template =
        include_str!("redirection/assets/redirect_error_logs_push.js").to_string();
    match form {
        RedirectForm::Form {
            endpoint,
            method,
            form_fields,
        } => maud::html! {
        (maud::DOCTYPE)
        html {
            meta name="viewport" content="width=device-width, initial-scale=1";
            head {
                style {
                    r##"

                    "##
                }
                (PreEscaped(r##"
                <style>
                    #loader1 {
                        width: 500px,
                    }
                    @media (max-width: 600px) {
                        #loader1 {
                            width: 200px
                        }
                    }
                </style>
                "##))
            }

            body style="background-color: #ffffff; padding: 20px; font-family: Arial, Helvetica, Sans-Serif;" {

                div id="loader1" class="lottie" style="height: 150px; display: block; position: relative; margin-left: auto; margin-right: auto;" { "" }

                (PreEscaped(r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/bodymovin/5.7.4/lottie.min.js"></script>"#))

                (PreEscaped(r#"
                <script>
                var anime = bodymovin.loadAnimation({
                    container: document.getElementById('loader1'),
                    renderer: 'svg',
                    loop: true,
                    autoplay: true,
                    name: 'hyperswitch loader',
                    animationData: {"v":"4.8.0","meta":{"g":"LottieFiles AE 3.1.1","a":"","k":"","d":"","tc":""},"fr":29.9700012207031,"ip":0,"op":31.0000012626559,"w":400,"h":250,"nm":"loader_shape","ddd":0,"assets":[],"layers":[{"ddd":0,"ind":1,"ty":4,"nm":"circle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[278.25,202.671,0],"ix":2},"a":{"a":0,"k":[23.72,23.72,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[12.935,0],[0,-12.936],[-12.935,0],[0,12.935]],"o":[[-12.952,0],[0,12.935],[12.935,0],[0,-12.936]],"v":[[0,-23.471],[-23.47,0.001],[0,23.471],[23.47,0.001]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":10,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":19.99,"s":[100]},{"t":29.9800012211104,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[23.72,23.721],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0},{"ddd":0,"ind":2,"ty":4,"nm":"square 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[196.25,201.271,0],"ix":2},"a":{"a":0,"k":[22.028,22.03,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[1.914,0],[0,0],[0,-1.914],[0,0],[-1.914,0],[0,0],[0,1.914],[0,0]],"o":[[0,0],[-1.914,0],[0,0],[0,1.914],[0,0],[1.914,0],[0,0],[0,-1.914]],"v":[[18.313,-21.779],[-18.312,-21.779],[-21.779,-18.313],[-21.779,18.314],[-18.312,21.779],[18.313,21.779],[21.779,18.314],[21.779,-18.313]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":5,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":14.99,"s":[100]},{"t":24.9800010174563,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[22.028,22.029],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":47.0000019143492,"st":0,"bm":0},{"ddd":0,"ind":3,"ty":4,"nm":"Triangle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[116.25,200.703,0],"ix":2},"a":{"a":0,"k":[27.11,21.243,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[0,0],[0.558,-0.879],[0,0],[-1.133,0],[0,0],[0.609,0.947],[0,0]],"o":[[-0.558,-0.879],[0,0],[-0.609,0.947],[0,0],[1.133,0],[0,0],[0,0]],"v":[[1.209,-20.114],[-1.192,-20.114],[-26.251,18.795],[-25.051,20.993],[25.051,20.993],[26.251,18.795],[1.192,-20.114]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":0,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":9.99,"s":[100]},{"t":19.9800008138021,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[27.11,21.243],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0}],"markers":[]}
                })
                </script>
                "#))

                h3 style="text-align: center;" { "Please wait while we process your payment..." }
                    form action=(PreEscaped(endpoint)) method=(method.to_string()) #payment_form {
                        @for (field, value) in form_fields {
                        input type="hidden" name=(field) value=(value);
                    }
                }
                (PreEscaped(format!(r#"
                    <script type="text/javascript"> {logging_template}
                    var frm = document.getElementById("payment_form");
                    var formFields = frm.querySelectorAll("input");

                    if (frm.method.toUpperCase() === "GET" && formFields.length === 0) {{
                        window.setTimeout(function () {{
                            window.location.href = frm.action;
                        }}, 300);
                    }} else {{
                        window.setTimeout(function () {{
                            frm.submit();
                        }}, 300);
                    }}
                    </script>
                    "#)))

            }
        }
        },
        RedirectForm::Html { html_data } => {
            PreEscaped(format!("{html_data} <script>{logging_template}</script>"))
        }
        RedirectForm::BlueSnap {
            payment_fields_token,
        } => {
            let card_details = if let Some(PaymentMethodData::Card(ccard)) = payment_method_data {
                format!(
                    "var saveCardDirectly={{cvv: \"{}\",amount: {},currency: \"{}\"}};",
                    ccard.card_cvc.peek(),
                    amount,
                    currency
                )
            } else {
                "".to_string()
            };
            let bluesnap_sdk_url = config.connectors.bluesnap.secondary_base_url;
            maud::html! {
            (maud::DOCTYPE)
            html {
                head {
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    (PreEscaped(format!("<script src=\"{bluesnap_sdk_url}web-sdk/5/bluesnap.js\"></script>")))
                }
                    body style="background-color: #ffffff; padding: 20px; font-family: Arial, Helvetica, Sans-Serif;" {

                        div id="loader1" class="lottie" style="height: 150px; display: block; position: relative; margin-top: 150px; margin-left: auto; margin-right: auto;" { "" }

                        (PreEscaped(r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/bodymovin/5.7.4/lottie.min.js"></script>"#))

                        (PreEscaped(r#"
                        <script>
                        var anime = bodymovin.loadAnimation({
                            container: document.getElementById('loader1'),
                            renderer: 'svg',
                            loop: true,
                            autoplay: true,
                            name: 'hyperswitch loader',
                            animationData: {"v":"4.8.0","meta":{"g":"LottieFiles AE 3.1.1","a":"","k":"","d":"","tc":""},"fr":29.9700012207031,"ip":0,"op":31.0000012626559,"w":400,"h":250,"nm":"loader_shape","ddd":0,"assets":[],"layers":[{"ddd":0,"ind":1,"ty":4,"nm":"circle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[278.25,202.671,0],"ix":2},"a":{"a":0,"k":[23.72,23.72,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[12.935,0],[0,-12.936],[-12.935,0],[0,12.935]],"o":[[-12.952,0],[0,12.935],[12.935,0],[0,-12.936]],"v":[[0,-23.471],[-23.47,0.001],[0,23.471],[23.47,0.001]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":10,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":19.99,"s":[100]},{"t":29.9800012211104,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[23.72,23.721],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0},{"ddd":0,"ind":2,"ty":4,"nm":"square 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[196.25,201.271,0],"ix":2},"a":{"a":0,"k":[22.028,22.03,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[1.914,0],[0,0],[0,-1.914],[0,0],[-1.914,0],[0,0],[0,1.914],[0,0]],"o":[[0,0],[-1.914,0],[0,0],[0,1.914],[0,0],[1.914,0],[0,0],[0,-1.914]],"v":[[18.313,-21.779],[-18.312,-21.779],[-21.779,-18.313],[-21.779,18.314],[-18.312,21.779],[18.313,21.779],[21.779,18.314],[21.779,-18.313]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":5,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":14.99,"s":[100]},{"t":24.9800010174563,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[22.028,22.029],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":47.0000019143492,"st":0,"bm":0},{"ddd":0,"ind":3,"ty":4,"nm":"Triangle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[116.25,200.703,0],"ix":2},"a":{"a":0,"k":[27.11,21.243,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[0,0],[0.558,-0.879],[0,0],[-1.133,0],[0,0],[0.609,0.947],[0,0]],"o":[[-0.558,-0.879],[0,0],[-0.609,0.947],[0,0],[1.133,0],[0,0],[0,0]],"v":[[1.209,-20.114],[-1.192,-20.114],[-26.251,18.795],[-25.051,20.993],[25.051,20.993],[26.251,18.795],[1.192,-20.114]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":0,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":9.99,"s":[100]},{"t":19.9800008138021,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[27.11,21.243],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0}],"markers":[]}
                        })
                        </script>
                        "#))

                        h3 style="text-align: center;" { "Please wait while we process your payment..." }
                    }

                (PreEscaped(format!("<script>
                    {logging_template}
                    bluesnap.threeDsPaymentsSetup(\"{payment_fields_token}\",
                    function(sdkResponse) {{
                        // console.log(sdkResponse);
                        var f = document.createElement('form');
                        f.action=window.location.pathname.replace(/payments\\/redirect\\/(\\w+)\\/(\\w+)\\/\\w+/, \"payments/$1/$2/redirect/complete/bluesnap?paymentToken={payment_fields_token}\");
                        f.method='POST';
                        var i=document.createElement('input');
                        i.type='hidden';
                        i.name='authentication_response';
                        i.value=JSON.stringify(sdkResponse);
                        f.appendChild(i);
                        document.body.appendChild(f);
                        f.submit();
                    }});
                    {card_details}
                    bluesnap.threeDsPaymentsSubmitData(saveCardDirectly);
                </script>
                ")))
                }}
        }
        RedirectForm::CybersourceAuthSetup {
            access_token,
            ddc_url,
            reference_id,
        } => {
            maud::html! {
            (maud::DOCTYPE)
            html {
                head {
                    meta name="viewport" content="width=device-width, initial-scale=1";
                }
                    body style="background-color: #ffffff; padding: 20px; font-family: Arial, Helvetica, Sans-Serif;" {

                        div id="loader1" class="lottie" style="height: 150px; display: block; position: relative; margin-top: 150px; margin-left: auto; margin-right: auto;" { "" }

                        (PreEscaped(r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/bodymovin/5.7.4/lottie.min.js"></script>"#))

                        (PreEscaped(r#"
                            <script>
                            var anime = bodymovin.loadAnimation({
                                container: document.getElementById('loader1'),
                                renderer: 'svg',
                                loop: true,
                                autoplay: true,
                                name: 'hyperswitch loader',
                                animationData: {"v":"4.8.0","meta":{"g":"LottieFiles AE 3.1.1","a":"","k":"","d":"","tc":""},"fr":29.9700012207031,"ip":0,"op":31.0000012626559,"w":400,"h":250,"nm":"loader_shape","ddd":0,"assets":[],"layers":[{"ddd":0,"ind":1,"ty":4,"nm":"circle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[278.25,202.671,0],"ix":2},"a":{"a":0,"k":[23.72,23.72,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[12.935,0],[0,-12.936],[-12.935,0],[0,12.935]],"o":[[-12.952,0],[0,12.935],[12.935,0],[0,-12.936]],"v":[[0,-23.471],[-23.47,0.001],[0,23.471],[23.47,0.001]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":10,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":19.99,"s":[100]},{"t":29.9800012211104,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[23.72,23.721],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0},{"ddd":0,"ind":2,"ty":4,"nm":"square 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[196.25,201.271,0],"ix":2},"a":{"a":0,"k":[22.028,22.03,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[1.914,0],[0,0],[0,-1.914],[0,0],[-1.914,0],[0,0],[0,1.914],[0,0]],"o":[[0,0],[-1.914,0],[0,0],[0,1.914],[0,0],[1.914,0],[0,0],[0,-1.914]],"v":[[18.313,-21.779],[-18.312,-21.779],[-21.779,-18.313],[-21.779,18.314],[-18.312,21.779],[18.313,21.779],[21.779,18.314],[21.779,-18.313]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":5,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":14.99,"s":[100]},{"t":24.9800010174563,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[22.028,22.029],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":47.0000019143492,"st":0,"bm":0},{"ddd":0,"ind":3,"ty":4,"nm":"Triangle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[116.25,200.703,0],"ix":2},"a":{"a":0,"k":[27.11,21.243,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[0,0],[0.558,-0.879],[0,0],[-1.133,0],[0,0],[0.609,0.947],[0,0]],"o":[[-0.558,-0.879],[0,0],[-0.609,0.947],[0,0],[1.133,0],[0,0],[0,0]],"v":[[1.209,-20.114],[-1.192,-20.114],[-26.251,18.795],[-25.051,20.993],[25.051,20.993],[26.251,18.795],[1.192,-20.114]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":0,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":9.99,"s":[100]},{"t":19.9800008138021,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[27.11,21.243],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0}],"markers":[]}
                            })
                            </script>
                            "#))

                        h3 style="text-align: center;" { "Please wait while we process your payment..." }
                    }

                (PreEscaped(r#"<iframe id="cardinal_collection_iframe" name="collectionIframe" height="10" width="10" style="display: none;"></iframe>"#))
                (PreEscaped(format!("<form id=\"cardinal_collection_form\" method=\"POST\" target=\"collectionIframe\" action=\"{ddc_url}\">
                <input id=\"cardinal_collection_form_input\" type=\"hidden\" name=\"JWT\" value=\"{access_token}\">
              </form>")))
              (PreEscaped(r#"<script>
              window.onload = function() {
              var cardinalCollectionForm = document.querySelector('#cardinal_collection_form'); if(cardinalCollectionForm) cardinalCollectionForm.submit();
              }
              </script>"#))
              (PreEscaped(format!("<script>
                {logging_template}
                window.addEventListener(\"message\", function(event) {{
                    if (event.origin === \"https://centinelapistag.cardinalcommerce.com\" || event.origin === \"https://centinelapi.cardinalcommerce.com\") {{
                      window.location.href = window.location.pathname.replace(/payments\\/redirect\\/(\\w+)\\/(\\w+)\\/\\w+/, \"payments/$1/$2/redirect/complete/cybersource?referenceId={reference_id}\");
                    }}
                  }}, false);
                </script>
                ")))
            }}
        }
        RedirectForm::CybersourceConsumerAuth {
            access_token,
            step_up_url,
        } => {
            maud::html! {
            (maud::DOCTYPE)
            html {
                head {
                    meta name="viewport" content="width=device-width, initial-scale=1";
                }
                    body style="background-color: #ffffff; padding: 20px; font-family: Arial, Helvetica, Sans-Serif;" {

                        div id="loader1" class="lottie" style="height: 150px; display: block; position: relative; margin-top: 150px; margin-left: auto; margin-right: auto;" { "" }

                        (PreEscaped(r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/bodymovin/5.7.4/lottie.min.js"></script>"#))

                        (PreEscaped(r#"
                            <script>
                            var anime = bodymovin.loadAnimation({
                                container: document.getElementById('loader1'),
                                renderer: 'svg',
                                loop: true,
                                autoplay: true,
                                name: 'hyperswitch loader',
                                animationData: {"v":"4.8.0","meta":{"g":"LottieFiles AE 3.1.1","a":"","k":"","d":"","tc":""},"fr":29.9700012207031,"ip":0,"op":31.0000012626559,"w":400,"h":250,"nm":"loader_shape","ddd":0,"assets":[],"layers":[{"ddd":0,"ind":1,"ty":4,"nm":"circle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[278.25,202.671,0],"ix":2},"a":{"a":0,"k":[23.72,23.72,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[12.935,0],[0,-12.936],[-12.935,0],[0,12.935]],"o":[[-12.952,0],[0,12.935],[12.935,0],[0,-12.936]],"v":[[0,-23.471],[-23.47,0.001],[0,23.471],[23.47,0.001]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":10,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":19.99,"s":[100]},{"t":29.9800012211104,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[23.72,23.721],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0},{"ddd":0,"ind":2,"ty":4,"nm":"square 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[196.25,201.271,0],"ix":2},"a":{"a":0,"k":[22.028,22.03,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[1.914,0],[0,0],[0,-1.914],[0,0],[-1.914,0],[0,0],[0,1.914],[0,0]],"o":[[0,0],[-1.914,0],[0,0],[0,1.914],[0,0],[1.914,0],[0,0],[0,-1.914]],"v":[[18.313,-21.779],[-18.312,-21.779],[-21.779,-18.313],[-21.779,18.314],[-18.312,21.779],[18.313,21.779],[21.779,18.314],[21.779,-18.313]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":5,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":14.99,"s":[100]},{"t":24.9800010174563,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[22.028,22.029],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":47.0000019143492,"st":0,"bm":0},{"ddd":0,"ind":3,"ty":4,"nm":"Triangle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[116.25,200.703,0],"ix":2},"a":{"a":0,"k":[27.11,21.243,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[0,0],[0.558,-0.879],[0,0],[-1.133,0],[0,0],[0.609,0.947],[0,0]],"o":[[-0.558,-0.879],[0,0],[-0.609,0.947],[0,0],[1.133,0],[0,0],[0,0]],"v":[[1.209,-20.114],[-1.192,-20.114],[-26.251,18.795],[-25.051,20.993],[25.051,20.993],[26.251,18.795],[1.192,-20.114]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":0,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":9.99,"s":[100]},{"t":19.9800008138021,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[27.11,21.243],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0}],"markers":[]}
                            })
                            </script>
                            "#))

                        h3 style="text-align: center;" { "Please wait while we process your payment..." }
                    }

                // This is the iframe recommended by cybersource but the redirection happens inside this iframe once otp
                // is received and we lose control of the redirection on user client browser, so to avoid that we have removed this iframe and directly consumed it.
                // (PreEscaped(r#"<iframe id="step_up_iframe" style="border: none; margin-left: auto; margin-right: auto; display: block" height="800px" width="400px" name="stepUpIframe"></iframe>"#))
                (PreEscaped(format!("<form id=\"step-up-form\" method=\"POST\" action=\"{step_up_url}\">
                <input type=\"hidden\" name=\"JWT\" value=\"{access_token}\">
              </form>")))
              (PreEscaped(format!("<script>
              {logging_template}
              window.onload = function() {{
              var stepUpForm = document.querySelector('#step-up-form'); if(stepUpForm) stepUpForm.submit();
              }}
              </script>")))
            }}
        }
        RedirectForm::DeutschebankThreeDSChallengeFlow { acs_url, creq } => {
            maud::html! {
                (maud::DOCTYPE)
                html {
                    head {
                        meta name="viewport" content="width=device-width, initial-scale=1";
                    }

                    body style="background-color: #ffffff; padding: 20px; font-family: Arial, Helvetica, Sans-Serif;" {
                        div id="loader1" class="lottie" style="height: 150px; display: block; position: relative; margin-top: 150px; margin-left: auto; margin-right: auto;" { "" }

                        (PreEscaped(r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/bodymovin/5.7.4/lottie.min.js"></script>"#))

                        (PreEscaped(r#"
                            <script>
                            var anime = bodymovin.loadAnimation({
                                container: document.getElementById('loader1'),
                                renderer: 'svg',
                                loop: true,
                                autoplay: true,
                                name: 'hyperswitch loader',
                                animationData: {"v":"4.8.0","meta":{"g":"LottieFiles AE 3.1.1","a":"","k":"","d":"","tc":""},"fr":29.9700012207031,"ip":0,"op":31.0000012626559,"w":400,"h":250,"nm":"loader_shape","ddd":0,"assets":[],"layers":[{"ddd":0,"ind":1,"ty":4,"nm":"circle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[278.25,202.671,0],"ix":2},"a":{"a":0,"k":[23.72,23.72,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[12.935,0],[0,-12.936],[-12.935,0],[0,12.935]],"o":[[-12.952,0],[0,12.935],[12.935,0],[0,-12.936]],"v":[[0,-23.471],[-23.47,0.001],[0,23.471],[23.47,0.001]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":10,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":19.99,"s":[100]},{"t":29.9800012211104,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[23.72,23.721],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0},{"ddd":0,"ind":2,"ty":4,"nm":"square 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[196.25,201.271,0],"ix":2},"a":{"a":0,"k":[22.028,22.03,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[1.914,0],[0,0],[0,-1.914],[0,0],[-1.914,0],[0,0],[0,1.914],[0,0]],"o":[[0,0],[-1.914,0],[0,0],[0,1.914],[0,0],[1.914,0],[0,0],[0,-1.914]],"v":[[18.313,-21.779],[-18.312,-21.779],[-21.779,-18.313],[-21.779,18.314],[-18.312,21.779],[18.313,21.779],[21.779,18.314],[21.779,-18.313]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":5,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":14.99,"s":[100]},{"t":24.9800010174563,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[22.028,22.029],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":47.0000019143492,"st":0,"bm":0},{"ddd":0,"ind":3,"ty":4,"nm":"Triangle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[116.25,200.703,0],"ix":2},"a":{"a":0,"k":[27.11,21.243,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[0,0],[0.558,-0.879],[0,0],[-1.133,0],[0,0],[0.609,0.947],[0,0]],"o":[[-0.558,-0.879],[0,0],[-0.609,0.947],[0,0],[1.133,0],[0,0],[0,0]],"v":[[1.209,-20.114],[-1.192,-20.114],[-26.251,18.795],[-25.051,20.993],[25.051,20.993],[26.251,18.795],[1.192,-20.114]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":0,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":9.99,"s":[100]},{"t":19.9800008138021,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[27.11,21.243],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0}],"markers":[]}
                            })
                            </script>
                            "#))

                        h3 style="text-align: center;" { "Please wait while we process your payment..." }
                    }
                    (PreEscaped(format!("<form id=\"PaReqForm\" method=\"POST\" action=\"{acs_url}\">
                        <input type=\"hidden\" name=\"creq\" value=\"{creq}\">
                        </form>")))
                    (PreEscaped(format!("<script>
                        {logging_template}
                        window.onload = function() {{
                        var paReqForm = document.querySelector('#PaReqForm'); if(paReqForm) paReqForm.submit();
                        }}
                    </script>")))
                }
            }
        }
        RedirectForm::Payme => {
            maud::html! {
                (maud::DOCTYPE)
                head {
                    (PreEscaped(r#"<script src="https://cdn.paymeservice.com/hf/v1/hostedfields.js"></script>"#))
                }
                (PreEscaped(format!("<script>
                    {logging_template}
                    var f = document.createElement('form');
                    f.action=window.location.pathname.replace(/payments\\/redirect\\/(\\w+)\\/(\\w+)\\/\\w+/, \"payments/$1/$2/redirect/complete/payme\");
                    f.method='POST';
                    PayMe.clientData()
                    .then((data) => {{
                        var i=document.createElement('input');
                        i.type='hidden';
                        i.name='meta_data';
                        i.value=data.hash;
                        f.appendChild(i);
                        document.body.appendChild(f);
                        f.submit();
                    }})
                    .catch((error) => {{
                        f.submit();
                    }});
            </script>
                ")))
            }
        }
        RedirectForm::Braintree {
            client_token,
            card_token,
            bin,
            acs_url,
        } => {
            maud::html! {
            (maud::DOCTYPE)
            html {
                head {
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    (PreEscaped(r#"<script src="https://js.braintreegateway.com/web/3.97.1/js/three-d-secure.js"></script>"#))
                    // (PreEscaped(r#"<script src="https://js.braintreegateway.com/web/3.97.1/js/hosted-fields.js"></script>"#))

                }
                    body style="background-color: #ffffff; padding: 20px; font-family: Arial, Helvetica, Sans-Serif;" {

                        div id="loader1" class="lottie" style="height: 150px; display: block; position: relative; margin-top: 150px; margin-left: auto; margin-right: auto;" { "" }

                        (PreEscaped(r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/bodymovin/5.7.4/lottie.min.js"></script>"#))

                        (PreEscaped(r#"
                            <script>
                            var anime = bodymovin.loadAnimation({
                                container: document.getElementById('loader1'),
                                renderer: 'svg',
                                loop: true,
                                autoplay: true,
                                name: 'hyperswitch loader',
                                animationData: {"v":"4.8.0","meta":{"g":"LottieFiles AE 3.1.1","a":"","k":"","d":"","tc":""},"fr":29.9700012207031,"ip":0,"op":31.0000012626559,"w":400,"h":250,"nm":"loader_shape","ddd":0,"assets":[],"layers":[{"ddd":0,"ind":1,"ty":4,"nm":"circle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[278.25,202.671,0],"ix":2},"a":{"a":0,"k":[23.72,23.72,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[12.935,0],[0,-12.936],[-12.935,0],[0,12.935]],"o":[[-12.952,0],[0,12.935],[12.935,0],[0,-12.936]],"v":[[0,-23.471],[-23.47,0.001],[0,23.471],[23.47,0.001]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":10,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":19.99,"s":[100]},{"t":29.9800012211104,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[23.72,23.721],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0},{"ddd":0,"ind":2,"ty":4,"nm":"square 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[196.25,201.271,0],"ix":2},"a":{"a":0,"k":[22.028,22.03,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[1.914,0],[0,0],[0,-1.914],[0,0],[-1.914,0],[0,0],[0,1.914],[0,0]],"o":[[0,0],[-1.914,0],[0,0],[0,1.914],[0,0],[1.914,0],[0,0],[0,-1.914]],"v":[[18.313,-21.779],[-18.312,-21.779],[-21.779,-18.313],[-21.779,18.314],[-18.312,21.779],[18.313,21.779],[21.779,18.314],[21.779,-18.313]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":5,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":14.99,"s":[100]},{"t":24.9800010174563,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[22.028,22.029],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":47.0000019143492,"st":0,"bm":0},{"ddd":0,"ind":3,"ty":4,"nm":"Triangle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[116.25,200.703,0],"ix":2},"a":{"a":0,"k":[27.11,21.243,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[0,0],[0.558,-0.879],[0,0],[-1.133,0],[0,0],[0.609,0.947],[0,0]],"o":[[-0.558,-0.879],[0,0],[-0.609,0.947],[0,0],[1.133,0],[0,0],[0,0]],"v":[[1.209,-20.114],[-1.192,-20.114],[-26.251,18.795],[-25.051,20.993],[25.051,20.993],[26.251,18.795],[1.192,-20.114]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":0,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":9.99,"s":[100]},{"t":19.9800008138021,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[27.11,21.243],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0}],"markers":[]}
                            })
                            </script>
                            "#))

                        h3 style="text-align: center;" { "Please wait while we process your payment..." }
                    }

                (PreEscaped(format!("<script>
                                {logging_template}
                                var my3DSContainer;
                                var clientToken = \"{client_token}\";
                                braintree.threeDSecure.create({{
                                        authorization: clientToken,
                                        version: 2
                                    }}, function(err, threeDs) {{
                                        threeDs.verifyCard({{
                                            amount: \"{amount}\",
                                            nonce: \"{card_token}\",
                                            bin: \"{bin}\",
                                            addFrame: function(err, iframe) {{
                                                my3DSContainer = document.createElement('div');
                                                my3DSContainer.appendChild(iframe);
                                                document.body.appendChild(my3DSContainer);
                                            }},
                                            removeFrame: function() {{
                                                if(my3DSContainer && my3DSContainer.parentNode) {{
                                                    my3DSContainer.parentNode.removeChild(my3DSContainer);
                                                }}
                                            }},
                                            onLookupComplete: function(data, next) {{
                                                // console.log(\"onLookup Complete\", data);
                                                    next();
                                                }}
                                            }},
                                            function(err, payload) {{
                                                if(err) {{
                                                    console.error(err);
                                                    var f = document.createElement('form');
                                                    f.action=window.location.pathname.replace(/payments\\/redirect\\/(\\w+)\\/(\\w+)\\/\\w+/, \"payments/$1/$2/redirect/response/braintree\");
                                                    var i = document.createElement('input');
                                                    i.type = 'hidden';
                                                    f.method='POST';
                                                    i.name = 'authentication_response';
                                                    i.value = JSON.stringify(err);
                                                    f.appendChild(i);
                                                    f.body = JSON.stringify(err);
                                                    document.body.appendChild(f);
                                                    f.submit();
                                                }} else {{
                                                    // console.log(payload);
                                                    var f = document.createElement('form');
                                                    f.action=\"{acs_url}\";
                                                    var i = document.createElement('input');
                                                    i.type = 'hidden';
                                                    f.method='POST';
                                                    i.name = 'authentication_response';
                                                    i.value = JSON.stringify(payload);
                                                    f.appendChild(i);
                                                    f.body = JSON.stringify(payload);
                                                    document.body.appendChild(f);
                                                    f.submit();
                                                    }}
                                                }});
                                        }}); </script>"
                                    )))
                }}
        }
        RedirectForm::Nmi {
            amount,
            currency,
            public_key,
            customer_vault_id,
            order_id,
        } => {
            let public_key_val = public_key.peek();
            maud::html! {
                    (maud::DOCTYPE)
                    head {
                        (PreEscaped(r#"<script src="https://secure.networkmerchants.com/js/v1/Gateway.js"></script>"#))
                    }
                    body style="background-color: #ffffff; padding: 20px; font-family: Arial, Helvetica, Sans-Serif;" {

                        div id="loader-wrapper" {
                            div id="loader1" class="lottie" style="height: 150px; display: block; position: relative; margin-top: 150px; margin-left: auto; margin-right: auto;" { "" }

                        (PreEscaped(r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/bodymovin/5.7.4/lottie.min.js"></script>"#))

                        (PreEscaped(r#"
                            <script>
                            var anime = bodymovin.loadAnimation({
                                container: document.getElementById('loader1'),
                                renderer: 'svg',
                                loop: true,
                                autoplay: true,
                                name: 'hyperswitch loader',
                                animationData: {"v":"4.8.0","meta":{"g":"LottieFiles AE 3.1.1","a":"","k":"","d":"","tc":""},"fr":29.9700012207031,"ip":0,"op":31.0000012626559,"w":400,"h":250,"nm":"loader_shape","ddd":0,"assets":[],"layers":[{"ddd":0,"ind":1,"ty":4,"nm":"circle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[278.25,202.671,0],"ix":2},"a":{"a":0,"k":[23.72,23.72,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[12.935,0],[0,-12.936],[-12.935,0],[0,12.935]],"o":[[-12.952,0],[0,12.935],[12.935,0],[0,-12.936]],"v":[[0,-23.471],[-23.47,0.001],[0,23.471],[23.47,0.001]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":10,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":19.99,"s":[100]},{"t":29.9800012211104,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[23.72,23.721],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0},{"ddd":0,"ind":2,"ty":4,"nm":"square 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[196.25,201.271,0],"ix":2},"a":{"a":0,"k":[22.028,22.03,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[1.914,0],[0,0],[0,-1.914],[0,0],[-1.914,0],[0,0],[0,1.914],[0,0]],"o":[[0,0],[-1.914,0],[0,0],[0,1.914],[0,0],[1.914,0],[0,0],[0,-1.914]],"v":[[18.313,-21.779],[-18.312,-21.779],[-21.779,-18.313],[-21.779,18.314],[-18.312,21.779],[18.313,21.779],[21.779,18.314],[21.779,-18.313]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":5,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":14.99,"s":[100]},{"t":24.9800010174563,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[22.028,22.029],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":47.0000019143492,"st":0,"bm":0},{"ddd":0,"ind":3,"ty":4,"nm":"Triangle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[116.25,200.703,0],"ix":2},"a":{"a":0,"k":[27.11,21.243,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[0,0],[0.558,-0.879],[0,0],[-1.133,0],[0,0],[0.609,0.947],[0,0]],"o":[[-0.558,-0.879],[0,0],[-0.609,0.947],[0,0],[1.133,0],[0,0],[0,0]],"v":[[1.209,-20.114],[-1.192,-20.114],[-26.251,18.795],[-25.051,20.993],[25.051,20.993],[26.251,18.795],[1.192,-20.114]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":0,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":9.99,"s":[100]},{"t":19.9800008138021,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[27.11,21.243],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0}],"markers":[]}
                            })
                            </script>
                            "#))

                        h3 style="text-align: center;" { "Please wait while we process your payment..." }
                        }

                        div id="threeds-wrapper" style="display: flex; width: 100%; height: 100vh; align-items: center; justify-content: center;" {""}
                    }
                    (PreEscaped(format!("<script>
                    {logging_template}
                    const gateway = Gateway.create('{public_key_val}');

                    // Initialize the ThreeDSService
                    const threeDS = gateway.get3DSecure();

                    const options = {{
                        customerVaultId: '{customer_vault_id}',
                        currency: '{currency}',
                        amount: '{amount}'
                    }};

                    const threeDSsecureInterface = threeDS.createUI(options);

                    threeDSsecureInterface.on('challenge', function(e) {{
                        document.getElementById('loader-wrapper').style.display = 'none';
                    }});

                    threeDSsecureInterface.on('complete', function(e) {{
                        var responseForm = document.createElement('form');
                        responseForm.action=window.location.pathname.replace(/payments\\/redirect\\/(\\w+)\\/(\\w+)\\/\\w+/, \"payments/$1/$2/redirect/complete/nmi\");
                        responseForm.method='POST';

                        var item1=document.createElement('input');
                        item1.type='hidden';
                        item1.name='cavv';
                        item1.value=e.cavv;
                        responseForm.appendChild(item1);

                        var item2=document.createElement('input');
                        item2.type='hidden';
                        item2.name='xid';
                        item2.value=e.xid;
                        responseForm.appendChild(item2);

                        var item6=document.createElement('input');
                        item6.type='hidden';
                        item6.name='eci';
                        item6.value=e.eci;
                        responseForm.appendChild(item6);

                        var item7=document.createElement('input');
                        item7.type='hidden';
                        item7.name='directoryServerId';
                        item7.value=e.directoryServerId;
                        responseForm.appendChild(item7);

                        var item3=document.createElement('input');
                        item3.type='hidden';
                        item3.name='cardHolderAuth';
                        item3.value=e.cardHolderAuth;
                        responseForm.appendChild(item3);

                        var item4=document.createElement('input');
                        item4.type='hidden';
                        item4.name='threeDsVersion';
                        item4.value=e.threeDsVersion;
                        responseForm.appendChild(item4);

                        var item5=document.createElement('input');
                        item5.type='hidden';
                        item5.name='orderId';
                        item5.value='{order_id}';
                        responseForm.appendChild(item5);

                        var item6=document.createElement('input');
                        item6.type='hidden';
                        item6.name='customerVaultId';
                        item6.value='{customer_vault_id}';
                        responseForm.appendChild(item6);

                        document.body.appendChild(responseForm);
                        responseForm.submit();
                    }});

                    threeDSsecureInterface.on('failure', function(e) {{
                        var responseForm = document.createElement('form');
                        responseForm.action=window.location.pathname.replace(/payments\\/redirect\\/(\\w+)\\/(\\w+)\\/\\w+/, \"payments/$1/$2/redirect/complete/nmi\");
                        responseForm.method='POST';

                        var error_code=document.createElement('input');
                        error_code.type='hidden';
                        error_code.name='code';
                        error_code.value= e.code;
                        responseForm.appendChild(error_code);

                        var error_message=document.createElement('input');
                        error_message.type='hidden';
                        error_message.name='message';
                        error_message.value= e.message;
                        responseForm.appendChild(error_message);

                        document.body.appendChild(responseForm);
                        responseForm.submit();
                    }});

                    threeDSsecureInterface.start('#threeds-wrapper');
            </script>"
            )))
                }
        }
        RedirectForm::Mifinity {
            initialization_token,
        } => {
            let mifinity_base_url = config.connectors.mifinity.base_url;
            maud::html! {
                        (maud::DOCTYPE)
                        head {
                            (PreEscaped(format!(r#"<script src='{mifinity_base_url}widgets/sgpg.js?58190a411dc3'></script>"#)))
                        }

                        (PreEscaped(format!("<div id='widget-container'></div>
	  <script>
		  var widget = showPaymentIframe('widget-container', {{
			  token: '{initialization_token}',
			  complete: function() {{
                var f = document.createElement('form');
                f.action=window.location.pathname.replace(/payments\\/redirect\\/(\\w+)\\/(\\w+)\\/\\w+/, \"payments/$1/$2/redirect/response/mifinity\");
                f.method='GET';
                document.body.appendChild(f);
                f.submit();
			  }}
		   }});
	   </script>")))

            }
        }
        RedirectForm::WorldpayDDCForm {
            endpoint,
            method,
            form_fields,
            collection_id,
        } => maud::html! {
            (maud::DOCTYPE)
            html {
                meta name="viewport" content="width=device-width, initial-scale=1";
                head {
                    (PreEscaped(r##"
                            <style>
                                #loader1 {
                                    width: 500px;
                                }
                                @media (max-width: 600px) {
                                    #loader1 {
                                        width: 200px;
                                    }
                                }
                            </style>
                        "##))
                }

                body style="background-color: #ffffff; padding: 20px; font-family: Arial, Helvetica, Sans-Serif;" {
                    div id="loader1" class="lottie" style="height: 150px; display: block; position: relative; margin-left: auto; margin-right: auto;" { "" }
                    (PreEscaped(r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/bodymovin/5.7.4/lottie.min.js"></script>"#))
                    (PreEscaped(r#"
                        <script>
                            var anime = bodymovin.loadAnimation({
                                container: document.getElementById('loader1'),
                                renderer: 'svg',
                                loop: true,
                                autoplay: true,
                                name: 'hyperswitch loader',
                                animationData: {"v":"4.8.0","meta":{"g":"LottieFiles AE 3.1.1","a":"","k":"","d":"","tc":""},"fr":29.9700012207031,"ip":0,"op":31.0000012626559,"w":400,"h":250,"nm":"loader_shape","ddd":0,"assets":[],"layers":[{"ddd":0,"ind":1,"ty":4,"nm":"circle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[278.25,202.671,0],"ix":2},"a":{"a":0,"k":[23.72,23.72,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[12.935,0],[0,-12.936],[-12.935,0],[0,12.935]],"o":[[-12.952,0],[0,12.935],[12.935,0],[0,-12.936]],"v":[[0,-23.471],[-23.47,0.001],[0,23.471],[23.47,0.001]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":10,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":19.99,"s":[100]},{"t":29.9800012211104,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[23.72,23.721],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0},{"ddd":0,"ind":2,"ty":4,"nm":"square 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[196.25,201.271,0],"ix":2},"a":{"a":0,"k":[22.028,22.03,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[1.914,0],[0,0],[0,-1.914],[0,0],[-1.914,0],[0,0],[0,1.914],[0,0]],"o":[[0,0],[-1.914,0],[0,0],[0,1.914],[0,0],[1.914,0],[0,0],[0,-1.914]],"v":[[18.313,-21.779],[-18.312,-21.779],[-21.779,-18.313],[-21.779,18.314],[-18.312,21.779],[18.313,21.779],[21.779,18.314],[21.779,-18.313]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":5,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":14.99,"s":[100]},{"t":24.9800010174563,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[22.028,22.029],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":47.0000019143492,"st":0,"bm":0},{"ddd":0,"ind":3,"ty":4,"nm":"Triangle 2","sr":1,"ks":{"o":{"a":0,"k":100,"ix":11},"r":{"a":0,"k":0,"ix":10},"p":{"a":0,"k":[116.25,200.703,0],"ix":2},"a":{"a":0,"k":[27.11,21.243,0],"ix":1},"s":{"a":0,"k":[100,100,100],"ix":6}},"ao":0,"shapes":[{"ty":"gr","it":[{"ind":0,"ty":"sh","ix":1,"ks":{"a":0,"k":{"i":[[0,0],[0.558,-0.879],[0,0],[-1.133,0],[0,0],[0.609,0.947],[0,0]],"o":[[-0.558,-0.879],[0,0],[-0.609,0.947],[0,0],[1.133,0],[0,0],[0,0]],"v":[[1.209,-20.114],[-1.192,-20.114],[-26.251,18.795],[-25.051,20.993],[25.051,20.993],[26.251,18.795],[1.192,-20.114]],"c":true},"ix":2},"nm":"Path 1","mn":"ADBE Vector Shape - Group","hd":false},{"ty":"fl","c":{"a":0,"k":[0,0.427451010311,0.976470648074,1],"ix":4},"o":{"a":1,"k":[{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":0,"s":[10]},{"i":{"x":[0.667],"y":[1]},"o":{"x":[0.333],"y":[0]},"t":9.99,"s":[100]},{"t":19.9800008138021,"s":[10]}],"ix":5},"r":1,"bm":0,"nm":"Fill 1","mn":"ADBE Vector Graphic - Fill","hd":false},{"ty":"tr","p":{"a":0,"k":[27.11,21.243],"ix":2},"a":{"a":0,"k":[0,0],"ix":1},"s":{"a":0,"k":[100,100],"ix":3},"r":{"a":0,"k":0,"ix":6},"o":{"a":0,"k":100,"ix":7},"sk":{"a":0,"k":0,"ix":4},"sa":{"a":0,"k":0,"ix":5},"nm":"Transform"}],"nm":"Group 1","np":2,"cix":2,"bm":0,"ix":1,"mn":"ADBE Vector Group","hd":false}],"ip":0,"op":48.0000019550801,"st":0,"bm":0}],"markers":[]}
                            })
                        </script>
                    "#))
                    h3 style="text-align: center;" { "Please wait while we process your payment..." }

                    script {
                        (PreEscaped(format!(
                            r#"
                                var ddcProcessed = false;
                                var timeoutHandle = null;
                                
                                function submitCollectionReference(collectionReference) {{
                                    if (ddcProcessed) {{
                                        console.log("DDC already processed, ignoring duplicate submission");
                                        return;
                                    }}
                                    ddcProcessed = true;
                                    
                                    if (timeoutHandle) {{
                                        clearTimeout(timeoutHandle);
                                        timeoutHandle = null;
                                    }}
                                    
                                    var redirectPathname = window.location.pathname.replace(/payments\/redirect\/([^\/]+)\/([^\/]+)\/[^\/]+/, "payments/$1/$2/redirect/complete/worldpay");
                                    var redirectUrl = window.location.origin + redirectPathname;
                                    try {{
                                        if (typeof collectionReference === "string" && collectionReference.length > 0) {{
                                            var form = document.createElement("form");
                                            form.action = redirectPathname;
                                            form.method = "GET";
                                            var input = document.createElement("input");
                                            input.type = "hidden";
                                            input.name = "collectionReference";
                                            input.value = collectionReference;
                                            form.appendChild(input);
                                            document.body.appendChild(form);
                                            form.submit();
                                        }} else {{
                                            window.location.replace(redirectUrl);
                                        }}
                                    }} catch (error) {{
                                        console.error("Error submitting DDC:", error);
                                        window.location.replace(redirectUrl);
                                    }}
                                }}
                                var allowedHost = "{}";
                                var collectionField = "{}";
                                window.addEventListener("message", function(event) {{
                                    if (ddcProcessed) {{
                                        console.log("DDC already processed, ignoring message event");
                                        return;
                                    }}
                                    if (event.origin === allowedHost) {{
                                        try {{
                                            var data = JSON.parse(event.data);
                                            if (collectionField.length > 0) {{
                                                var collectionReference = data[collectionField];
                                                return submitCollectionReference(collectionReference);
                                            }} else {{
                                                console.error("Collection field not found in event data (" + collectionField + ")");
                                            }}
                                        }} catch (error) {{
                                            console.error("Error parsing event data: ", error);
                                        }}
                                    }} else {{
                                        console.error("Invalid origin: " + event.origin, "Expected origin: " + allowedHost);
                                    }}

                                    submitCollectionReference("");
                                }});

                                // Timeout after 10 seconds and will submit empty collection reference
                                timeoutHandle = window.setTimeout(function() {{
                                    if (!ddcProcessed) {{
                                        console.log("DDC timeout reached, submitting empty collection reference");
                                        submitCollectionReference("");
                                    }}
                                }}, 10000);
                            "#,
                            endpoint.host_str().map_or(endpoint.as_ref().split('/').take(3).collect::<Vec<&str>>().join("/"), |host| format!("{}://{}", endpoint.scheme(), host)),
                            collection_id.clone().unwrap_or("".to_string())))
                        )
                    }

                    iframe
                        style="display: none;"
                        srcdoc=(
                            maud::html! {
                                (maud::DOCTYPE)
                                html {
                                    body {
                                        form action=(PreEscaped(endpoint.to_string())) method=(method.to_string()) #payment_form {
                                            @for (field, value) in form_fields {
                                                input type="hidden" name=(field) value=(value);
                                            }
                                        }
                                        (PreEscaped(format!(r#"
                                            <script type="text/javascript"> {logging_template}
                                                var form = document.getElementById("payment_form");
                                                var formFields = form.querySelectorAll("input");
                                                window.setTimeout(function () {{
                                                    if (form.method.toUpperCase() === "GET" && formFields.length === 0) {{
                                                        window.location.href = form.action;
                                                    }} else {{
                                                        form.submit();
                                                    }}
                                                }}, 300);
                                            </script>
                                        "#)))
                                    }
                                }
                            }.into_string()
                        )
                        {}
                }
            }
        },
    }
}

fn build_payment_link_template(
    payment_link_data: PaymentLinkFormData,
) -> CustomResult<(Tera, Context), errors::ApiErrorResponse> {
    let mut tera = Tera::default();

    // Add modification to css template with dynamic data
    let css_template =
        include_str!("../core/payment_link/payment_link_initiate/payment_link.css").to_string();
    let _ = tera.add_raw_template("payment_link_css", &css_template);
    let mut context = Context::new();
    context.insert("css_color_scheme", &payment_link_data.css_script);

    let rendered_css = match tera.render("payment_link_css", &context) {
        Ok(rendered_css) => rendered_css,
        Err(tera_error) => {
            crate::logger::warn!("{tera_error}");
            Err(errors::ApiErrorResponse::InternalServerError)?
        }
    };

    // Add modification to js template with dynamic data
    let js_template =
        include_str!("../core/payment_link/payment_link_initiate/payment_link.js").to_string();

    let _ = tera.add_raw_template("payment_link_js", &js_template);

    context.insert("payment_details_js_script", &payment_link_data.js_script);
    let sdk_origin = payment_link_data
        .sdk_url
        .host_str()
        .ok_or_else(|| {
            logger::error!("Host missing for payment link SDK URL");
            report!(errors::ApiErrorResponse::InternalServerError)
        })
        .and_then(|host| {
            if host == "localhost" {
                let port = payment_link_data.sdk_url.port().ok_or_else(|| {
                    logger::error!("Port missing for localhost in SDK URL");
                    report!(errors::ApiErrorResponse::InternalServerError)
                })?;
                Ok(format!(
                    "{}://{}:{}",
                    payment_link_data.sdk_url.scheme(),
                    host,
                    port
                ))
            } else {
                Ok(format!("{}://{}", payment_link_data.sdk_url.scheme(), host))
            }
        })?;
    context.insert("sdk_origin", &sdk_origin);

    let rendered_js = match tera.render("payment_link_js", &context) {
        Ok(rendered_js) => rendered_js,
        Err(tera_error) => {
            crate::logger::warn!("{tera_error}");
            Err(errors::ApiErrorResponse::InternalServerError)?
        }
    };

    // Logging template
    let logging_template =
        include_str!("redirection/assets/redirect_error_logs_push.js").to_string();
    //Locale template
    let locale_template = include_str!("../core/payment_link/locale.js").to_string();

    // Modify Html template with rendered js and rendered css files
    let html_template =
        include_str!("../core/payment_link/payment_link_initiate/payment_link.html").to_string();

    let _ = tera.add_raw_template("payment_link", &html_template);

    context.insert("rendered_meta_tag_html", &payment_link_data.html_meta_tags);

    context.insert(
        "preload_link_tags",
        &get_preload_link_html_template(&payment_link_data.sdk_url),
    );

    context.insert(
        "hyperloader_sdk_link",
        &get_hyper_loader_sdk(&payment_link_data.sdk_url),
    );
    context.insert("locale_template", &locale_template);
    context.insert("rendered_css", &rendered_css);
    context.insert("rendered_js", &rendered_js);

    context.insert("logging_template", &logging_template);

    Ok((tera, context))
}

pub fn build_payment_link_html(
    payment_link_data: PaymentLinkFormData,
) -> CustomResult<String, errors::ApiErrorResponse> {
    let (tera, mut context) = build_payment_link_template(payment_link_data)
        .attach_printable("Failed to build payment link's HTML template")?;
    let payment_link_initiator =
        include_str!("../core/payment_link/payment_link_initiate/payment_link_initiator.js")
            .to_string();
    context.insert("payment_link_initiator", &payment_link_initiator);

    tera.render("payment_link", &context)
        .map_err(|tera_error: TeraError| {
            crate::logger::warn!("{tera_error}");
            report!(errors::ApiErrorResponse::InternalServerError)
        })
        .attach_printable("Error while rendering open payment link's HTML template")
}

pub fn build_secure_payment_link_html(
    payment_link_data: PaymentLinkFormData,
) -> CustomResult<String, errors::ApiErrorResponse> {
    let (tera, mut context) = build_payment_link_template(payment_link_data)
        .attach_printable("Failed to build payment link's HTML template")?;
    let payment_link_initiator =
        include_str!("../core/payment_link/payment_link_initiate/secure_payment_link_initiator.js")
            .to_string();
    context.insert("payment_link_initiator", &payment_link_initiator);

    tera.render("payment_link", &context)
        .map_err(|tera_error: TeraError| {
            crate::logger::warn!("{tera_error}");
            report!(errors::ApiErrorResponse::InternalServerError)
        })
        .attach_printable("Error while rendering secure payment link's HTML template")
}

fn get_hyper_loader_sdk(sdk_url: &url::Url) -> String {
    format!("<script src=\"{sdk_url}\" onload=\"initializeSDK()\"></script>")
}

fn get_preload_link_html_template(sdk_url: &url::Url) -> String {
    format!(
        r#"<link rel="preload" href="https://fonts.googleapis.com/css2?family=Montserrat:wght@400;500;600;700;800" as="style">
            <link rel="preload" href="{sdk_url}" as="script">"#,
    )
}

pub fn get_payment_link_status(
    payment_link_data: PaymentLinkStatusData,
) -> CustomResult<String, errors::ApiErrorResponse> {
    let mut tera = Tera::default();

    // Add modification to css template with dynamic data
    let css_template =
        include_str!("../core/payment_link/payment_link_status/status.css").to_string();
    let _ = tera.add_raw_template("payment_link_css", &css_template);
    let mut context = Context::new();
    context.insert("css_color_scheme", &payment_link_data.css_script);

    let rendered_css = match tera.render("payment_link_css", &context) {
        Ok(rendered_css) => rendered_css,
        Err(tera_error) => {
            crate::logger::warn!("{tera_error}");
            Err(errors::ApiErrorResponse::InternalServerError)?
        }
    };

    //Locale template
    let locale_template = include_str!("../core/payment_link/locale.js");

    // Logging template
    let logging_template =
        include_str!("redirection/assets/redirect_error_logs_push.js").to_string();

    // Add modification to js template with dynamic data
    let js_template =
        include_str!("../core/payment_link/payment_link_status/status.js").to_string();
    let _ = tera.add_raw_template("payment_link_js", &js_template);
    context.insert("payment_details_js_script", &payment_link_data.js_script);

    let rendered_js = match tera.render("payment_link_js", &context) {
        Ok(rendered_js) => rendered_js,
        Err(tera_error) => {
            crate::logger::warn!("{tera_error}");
            Err(errors::ApiErrorResponse::InternalServerError)?
        }
    };

    // Modify Html template with rendered js and rendered css files
    let html_template =
        include_str!("../core/payment_link/payment_link_status/status.html").to_string();
    let _ = tera.add_raw_template("payment_link_status", &html_template);

    context.insert("rendered_css", &rendered_css);
    context.insert("locale_template", &locale_template);

    context.insert("rendered_js", &rendered_js);
    context.insert("logging_template", &logging_template);

    match tera.render("payment_link_status", &context) {
        Ok(rendered_html) => Ok(rendered_html),
        Err(tera_error) => {
            crate::logger::warn!("{tera_error}");
            Err(errors::ApiErrorResponse::InternalServerError)?
        }
    }
}

pub fn extract_mapped_fields(
    value: &serde_json::Value,
    mapping: Option<&HashMap<String, String>>,
    existing_enhancement: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let mapping = mapping?;

    if mapping.is_empty() {
        return existing_enhancement.cloned();
    }

    let mut enhancement = match existing_enhancement {
        Some(existing) if existing.is_object() => existing.clone(),
        _ => serde_json::json!({}),
    };

    for (dot_path, output_key) in mapping {
        if let Some(extracted_value) = extract_field_by_dot_path(value, dot_path) {
            if let Some(obj) = enhancement.as_object_mut() {
                obj.insert(output_key.clone(), extracted_value);
            }
        }
    }

    if enhancement.as_object().is_some_and(|obj| !obj.is_empty()) {
        Some(enhancement)
    } else {
        None
    }
}

pub fn extract_field_by_dot_path(
    value: &serde_json::Value,
    path: &str,
) -> Option<serde_json::Value> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;

    for part in parts {
        match current {
            serde_json::Value::Object(obj) => {
                current = obj.get(part)?;
            }
            serde_json::Value::Array(arr) => {
                // Try to parse part as array index
                if let Ok(index) = part.parse::<usize>() {
                    current = arr.get(index)?;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }

    Some(current.clone())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_mime_essence() {
        assert_eq!(mime::APPLICATION_JSON.essence_str(), "application/json");
    }
}
