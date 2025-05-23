pub mod fraud_check_post;
pub mod fraud_check_pre;
use async_trait::async_trait;
use common_enums::FrmSuggestion;
use error_stack::{report, ResultExt};

pub use self::{fraud_check_post::FraudCheckPost, fraud_check_pre::FraudCheckPre};
use super::{
    types::{ConnectorDetailsCore, FrmConfigsObject, PaymentToFrmData},
    FrmData,
};
use crate::{
    core::errors::{self, RouterResult},
    routes::{app::ReqState, SessionState},
    types::{domain, fraud_check::FrmRouterData},
};

pub type BoxedFraudCheckOperation<F, D> = Box<dyn FraudCheckOperation<F, D> + Send + Sync>;

pub trait FraudCheckOperation<F, D>: Send + std::fmt::Debug {
    fn to_get_tracker(&self) -> RouterResult<&(dyn GetTracker<PaymentToFrmData> + Send + Sync)> {
        Err(report!(errors::ApiErrorResponse::InternalServerError))
            .attach_printable_lazy(|| format!("get tracker interface not found for {self:?}"))
    }
    fn to_domain(&self) -> RouterResult<&(dyn Domain<F, D>)> {
        Err(report!(errors::ApiErrorResponse::InternalServerError))
            .attach_printable_lazy(|| format!("domain interface not found for {self:?}"))
    }
    fn to_update_tracker(&self) -> RouterResult<&(dyn UpdateTracker<FrmData, F, D> + Send + Sync)> {
        Err(report!(errors::ApiErrorResponse::InternalServerError))
            .attach_printable_lazy(|| format!("get tracker interface not found for {self:?}"))
    }
}

#[async_trait]
pub trait GetTracker<D>: Send {
    async fn get_trackers<'a>(
        &'a self,
        state: &'a SessionState,
        payment_data: D,
        frm_connector_details: ConnectorDetailsCore,
    ) -> RouterResult<Option<FrmData>>;
}

#[async_trait]
#[allow(clippy::too_many_arguments)]
pub trait Domain<F, D>: Send + Sync {
    async fn post_payment_frm<'a>(
        &'a self,
        state: &'a SessionState,
        req_state: ReqState,
        payment_data: &mut D,
        frm_data: &mut FrmData,
        merchant_context: &domain::MerchantContext,
        customer: &Option<domain::Customer>,
    ) -> RouterResult<Option<FrmRouterData>>
    where
        F: Send + Clone;

    async fn pre_payment_frm<'a>(
        &'a self,
        state: &'a SessionState,
        payment_data: &mut D,
        frm_data: &mut FrmData,
        merchant_context: &domain::MerchantContext,
        customer: &Option<domain::Customer>,
    ) -> RouterResult<FrmRouterData>
    where
        F: Send + Clone;

    // To execute several tasks conditionally based on the result of post_flow.
    // Eg: If the /sale(post flow) is returning the transaction as fraud we can execute refund in post task
    #[allow(clippy::too_many_arguments)]
    async fn execute_post_tasks(
        &self,
        _state: &SessionState,
        _req_state: ReqState,
        frm_data: &mut FrmData,
        _merchant_context: &domain::MerchantContext,
        _frm_configs: FrmConfigsObject,
        _frm_suggestion: &mut Option<FrmSuggestion>,
        _payment_data: &mut D,
        _customer: &Option<domain::Customer>,
        _should_continue_capture: &mut bool,
    ) -> RouterResult<Option<FrmData>>
    where
        F: Send + Clone,
    {
        return Ok(Some(frm_data.to_owned()));
    }
}

#[async_trait]
pub trait UpdateTracker<Fd, F: Clone, D>: Send {
    async fn update_tracker<'b>(
        &'b self,
        state: &SessionState,
        key_store: &domain::MerchantKeyStore,
        frm_data: Fd,
        payment_data: &mut D,
        _frm_suggestion: Option<FrmSuggestion>,
        frm_router_data: FrmRouterData,
    ) -> RouterResult<Fd>;
}
