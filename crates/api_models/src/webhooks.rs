use common_utils::custom_serde;
use serde::{Deserialize, Serialize};
use time::PrimitiveDateTime;
use utoipa::ToSchema;

#[cfg(feature = "payouts")]
use crate::payouts;
use crate::{disputes, enums as api_enums, mandates, payments, refunds};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Copy)]
#[serde(rename_all = "snake_case")]
pub enum IncomingWebhookEvent {
    /// Authorization + Capture failure
    PaymentIntentFailure,
    /// Authorization + Capture success
    PaymentIntentSuccess,
    PaymentIntentProcessing,
    PaymentIntentPartiallyFunded,
    PaymentIntentCancelled,
    PaymentIntentCancelFailure,
    PaymentIntentAuthorizationSuccess,
    PaymentIntentAuthorizationFailure,
    PaymentIntentCaptureSuccess,
    PaymentIntentCaptureFailure,
    PaymentIntentExpired,
    PaymentActionRequired,
    EventNotSupported,
    SourceChargeable,
    SourceTransactionCreated,
    RefundFailure,
    RefundSuccess,
    DisputeOpened,
    DisputeExpired,
    DisputeAccepted,
    DisputeCancelled,
    DisputeChallenged,
    // dispute has been successfully challenged by the merchant
    DisputeWon,
    // dispute has been unsuccessfully challenged
    DisputeLost,
    MandateActive,
    MandateRevoked,
    EndpointVerification,
    ExternalAuthenticationARes,
    FrmApproved,
    FrmRejected,
    #[cfg(feature = "payouts")]
    PayoutSuccess,
    #[cfg(feature = "payouts")]
    PayoutFailure,
    #[cfg(feature = "payouts")]
    PayoutProcessing,
    #[cfg(feature = "payouts")]
    PayoutCancelled,
    #[cfg(feature = "payouts")]
    PayoutCreated,
    #[cfg(feature = "payouts")]
    PayoutExpired,
    #[cfg(feature = "payouts")]
    PayoutReversed,
    #[cfg(all(feature = "revenue_recovery", feature = "v2"))]
    RecoveryPaymentFailure,
    #[cfg(all(feature = "revenue_recovery", feature = "v2"))]
    RecoveryPaymentSuccess,
    #[cfg(all(feature = "revenue_recovery", feature = "v2"))]
    RecoveryPaymentPending,
    #[cfg(all(feature = "revenue_recovery", feature = "v2"))]
    RecoveryInvoiceCancel,
}

pub enum WebhookFlow {
    Payment,
    #[cfg(feature = "payouts")]
    Payout,
    Refund,
    Dispute,
    Subscription,
    ReturnResponse,
    BankTransfer,
    Mandate,
    ExternalAuthentication,
    FraudCheck,
    #[cfg(all(feature = "revenue_recovery", feature = "v2"))]
    Recovery,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
/// This enum tells about the affect a webhook had on an object
pub enum WebhookResponseTracker {
    #[cfg(feature = "v1")]
    Payment {
        payment_id: common_utils::id_type::PaymentId,
        status: common_enums::IntentStatus,
    },
    #[cfg(feature = "v2")]
    Payment {
        payment_id: common_utils::id_type::GlobalPaymentId,
        status: common_enums::IntentStatus,
    },
    #[cfg(feature = "payouts")]
    Payout {
        payout_id: common_utils::id_type::PayoutId,
        status: common_enums::PayoutStatus,
    },
    #[cfg(feature = "v1")]
    Refund {
        payment_id: common_utils::id_type::PaymentId,
        refund_id: String,
        status: common_enums::RefundStatus,
    },
    #[cfg(feature = "v2")]
    Refund {
        payment_id: common_utils::id_type::GlobalPaymentId,
        refund_id: String,
        status: common_enums::RefundStatus,
    },
    #[cfg(feature = "v1")]
    Dispute {
        dispute_id: String,
        payment_id: common_utils::id_type::PaymentId,
        status: common_enums::DisputeStatus,
    },
    #[cfg(feature = "v2")]
    Dispute {
        dispute_id: String,
        payment_id: common_utils::id_type::GlobalPaymentId,
        status: common_enums::DisputeStatus,
    },
    Mandate {
        mandate_id: String,
        status: common_enums::MandateStatus,
    },
    #[cfg(feature = "v1")]
    PaymentMethod {
        payment_method_id: String,
        status: common_enums::PaymentMethodStatus,
    },
    NoEffect,
    Relay {
        relay_id: common_utils::id_type::RelayId,
        status: common_enums::RelayStatus,
    },
}

impl WebhookResponseTracker {
    #[cfg(feature = "v1")]
    pub fn get_payment_id(&self) -> Option<common_utils::id_type::PaymentId> {
        match self {
            Self::Payment { payment_id, .. }
            | Self::Refund { payment_id, .. }
            | Self::Dispute { payment_id, .. } => Some(payment_id.to_owned()),
            Self::NoEffect | Self::Mandate { .. } | Self::PaymentMethod { .. } => None,
            #[cfg(feature = "payouts")]
            Self::Payout { .. } => None,
            Self::Relay { .. } => None,
        }
    }

    #[cfg(feature = "v1")]
    pub fn get_payment_method_id(&self) -> Option<String> {
        match self {
            Self::PaymentMethod {
                payment_method_id, ..
            } => Some(payment_method_id.to_owned()),
            Self::Payment { .. }
            | Self::Refund { .. }
            | Self::Dispute { .. }
            | Self::NoEffect
            | Self::Mandate { .. }
            | Self::Relay { .. } => None,
            #[cfg(feature = "payouts")]
            Self::Payout { .. } => None,
        }
    }

    #[cfg(feature = "v2")]
    pub fn get_payment_id(&self) -> Option<common_utils::id_type::GlobalPaymentId> {
        match self {
            Self::Payment { payment_id, .. }
            | Self::Refund { payment_id, .. }
            | Self::Dispute { payment_id, .. } => Some(payment_id.to_owned()),
            Self::NoEffect | Self::Mandate { .. } => None,
            #[cfg(feature = "payouts")]
            Self::Payout { .. } => None,
            Self::Relay { .. } => None,
        }
    }
}

impl From<IncomingWebhookEvent> for WebhookFlow {
    fn from(evt: IncomingWebhookEvent) -> Self {
        match evt {
            IncomingWebhookEvent::PaymentIntentFailure
            | IncomingWebhookEvent::PaymentIntentSuccess
            | IncomingWebhookEvent::PaymentIntentProcessing
            | IncomingWebhookEvent::PaymentActionRequired
            | IncomingWebhookEvent::PaymentIntentPartiallyFunded
            | IncomingWebhookEvent::PaymentIntentCancelled
            | IncomingWebhookEvent::PaymentIntentCancelFailure
            | IncomingWebhookEvent::PaymentIntentAuthorizationSuccess
            | IncomingWebhookEvent::PaymentIntentAuthorizationFailure
            | IncomingWebhookEvent::PaymentIntentCaptureSuccess
            | IncomingWebhookEvent::PaymentIntentCaptureFailure
            | IncomingWebhookEvent::PaymentIntentExpired => Self::Payment,
            IncomingWebhookEvent::EventNotSupported => Self::ReturnResponse,
            IncomingWebhookEvent::RefundSuccess | IncomingWebhookEvent::RefundFailure => {
                Self::Refund
            }
            IncomingWebhookEvent::MandateActive | IncomingWebhookEvent::MandateRevoked => {
                Self::Mandate
            }
            IncomingWebhookEvent::DisputeOpened
            | IncomingWebhookEvent::DisputeAccepted
            | IncomingWebhookEvent::DisputeExpired
            | IncomingWebhookEvent::DisputeCancelled
            | IncomingWebhookEvent::DisputeChallenged
            | IncomingWebhookEvent::DisputeWon
            | IncomingWebhookEvent::DisputeLost => Self::Dispute,
            IncomingWebhookEvent::EndpointVerification => Self::ReturnResponse,
            IncomingWebhookEvent::SourceChargeable
            | IncomingWebhookEvent::SourceTransactionCreated => Self::BankTransfer,
            IncomingWebhookEvent::ExternalAuthenticationARes => Self::ExternalAuthentication,
            IncomingWebhookEvent::FrmApproved | IncomingWebhookEvent::FrmRejected => {
                Self::FraudCheck
            }
            #[cfg(feature = "payouts")]
            IncomingWebhookEvent::PayoutSuccess
            | IncomingWebhookEvent::PayoutFailure
            | IncomingWebhookEvent::PayoutProcessing
            | IncomingWebhookEvent::PayoutCancelled
            | IncomingWebhookEvent::PayoutCreated
            | IncomingWebhookEvent::PayoutExpired
            | IncomingWebhookEvent::PayoutReversed => Self::Payout,
            #[cfg(all(feature = "revenue_recovery", feature = "v2"))]
            IncomingWebhookEvent::RecoveryInvoiceCancel
            | IncomingWebhookEvent::RecoveryPaymentFailure
            | IncomingWebhookEvent::RecoveryPaymentPending
            | IncomingWebhookEvent::RecoveryPaymentSuccess => Self::Recovery,
        }
    }
}

pub type MerchantWebhookConfig = std::collections::HashSet<IncomingWebhookEvent>;

#[derive(Clone)]
pub enum RefundIdType {
    RefundId(String),
    ConnectorRefundId(String),
}

#[derive(Clone)]
pub enum MandateIdType {
    MandateId(String),
    ConnectorMandateId(String),
}

#[derive(Clone)]
pub enum AuthenticationIdType {
    AuthenticationId(common_utils::id_type::AuthenticationId),
    ConnectorAuthenticationId(String),
}

#[cfg(feature = "payouts")]
#[derive(Clone)]
pub enum PayoutIdType {
    PayoutAttemptId(String),
    ConnectorPayoutId(String),
}

#[derive(Clone)]
pub enum ObjectReferenceId {
    PaymentId(payments::PaymentIdType),
    RefundId(RefundIdType),
    MandateId(MandateIdType),
    ExternalAuthenticationID(AuthenticationIdType),
    #[cfg(feature = "payouts")]
    PayoutId(PayoutIdType),
    #[cfg(all(feature = "revenue_recovery", feature = "v2"))]
    InvoiceId(InvoiceIdType),
}

#[cfg(all(feature = "revenue_recovery", feature = "v2"))]
#[derive(Clone)]
pub enum InvoiceIdType {
    ConnectorInvoiceId(String),
}

#[cfg(all(feature = "revenue_recovery", feature = "v2"))]
impl ObjectReferenceId {
    pub fn get_connector_transaction_id_as_string(
        self,
    ) -> Result<String, common_utils::errors::ValidationError> {
        match self {
            Self::PaymentId(
                payments::PaymentIdType::ConnectorTransactionId(id)
            ) => Ok(id),
            Self::PaymentId(_)=>Err(
                common_utils::errors::ValidationError::IncorrectValueProvided {
                    field_name: "ConnectorTransactionId variant of PaymentId is required but received otherr variant",
                },
            ),
            Self::RefundId(_) => Err(
                common_utils::errors::ValidationError::IncorrectValueProvided {
                    field_name: "PaymentId is required but received RefundId",
                },
            ),
            Self::MandateId(_) => Err(
                common_utils::errors::ValidationError::IncorrectValueProvided {
                    field_name: "PaymentId is required but received MandateId",
                },
            ),
            Self::ExternalAuthenticationID(_) => Err(
                common_utils::errors::ValidationError::IncorrectValueProvided {
                    field_name: "PaymentId is required but received ExternalAuthenticationID",
                },
            ),
            #[cfg(feature = "payouts")]
            Self::PayoutId(_) => Err(
                common_utils::errors::ValidationError::IncorrectValueProvided {
                    field_name: "PaymentId is required but received PayoutId",
                },
            ),
            Self::InvoiceId(_) => Err(
                common_utils::errors::ValidationError::IncorrectValueProvided {
                    field_name: "PaymentId is required but received InvoiceId",
                },
            )
        }
    }
}

pub struct IncomingWebhookDetails {
    pub object_reference_id: ObjectReferenceId,
    pub resource_object: Vec<u8>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OutgoingWebhook {
    /// The merchant id of the merchant
    #[schema(value_type = String)]
    pub merchant_id: common_utils::id_type::MerchantId,

    /// The unique event id for each webhook
    pub event_id: String,

    /// The type of event this webhook corresponds to.
    #[schema(value_type = EventType)]
    pub event_type: api_enums::EventType,

    /// This is specific to the flow, for ex: it will be `PaymentsResponse` for payments flow
    pub content: OutgoingWebhookContent,

    /// The time at which webhook was sent
    #[serde(default, with = "custom_serde::iso8601")]
    pub timestamp: PrimitiveDateTime,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type", content = "object", rename_all = "snake_case")]
#[cfg(feature = "v1")]
pub enum OutgoingWebhookContent {
    #[schema(value_type = PaymentsResponse, title = "PaymentsResponse")]
    PaymentDetails(Box<payments::PaymentsResponse>),
    #[schema(value_type = RefundResponse, title = "RefundResponse")]
    RefundDetails(Box<refunds::RefundResponse>),
    #[schema(value_type = DisputeResponse, title = "DisputeResponse")]
    DisputeDetails(Box<disputes::DisputeResponse>),
    #[schema(value_type = MandateResponse, title = "MandateResponse")]
    MandateDetails(Box<mandates::MandateResponse>),
    #[cfg(feature = "payouts")]
    #[schema(value_type = PayoutCreateResponse, title = "PayoutCreateResponse")]
    PayoutDetails(Box<payouts::PayoutCreateResponse>),
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type", content = "object", rename_all = "snake_case")]
#[cfg(feature = "v2")]
pub enum OutgoingWebhookContent {
    #[schema(value_type = PaymentsResponse, title = "PaymentsResponse")]
    PaymentDetails(Box<payments::PaymentsResponse>),
    #[schema(value_type = RefundResponse, title = "RefundResponse")]
    RefundDetails(Box<refunds::RefundResponse>),
    #[schema(value_type = DisputeResponse, title = "DisputeResponse")]
    DisputeDetails(Box<disputes::DisputeResponse>),
    #[schema(value_type = MandateResponse, title = "MandateResponse")]
    MandateDetails(Box<mandates::MandateResponse>),
    #[cfg(feature = "payouts")]
    #[schema(value_type = PayoutCreateResponse, title = "PayoutCreateResponse")]
    PayoutDetails(Box<payouts::PayoutCreateResponse>),
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectorWebhookSecrets {
    pub secret: Vec<u8>,
    pub additional_secret: Option<masking::Secret<String>>,
}

#[cfg(all(feature = "v2", feature = "revenue_recovery"))]
impl IncomingWebhookEvent {
    pub fn is_recovery_transaction_event(&self) -> bool {
        matches!(
            self,
            Self::RecoveryPaymentFailure
                | Self::RecoveryPaymentSuccess
                | Self::RecoveryPaymentPending
        )
    }
}
