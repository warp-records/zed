use anyhow::{Context as _, bail};
use axum::routing::put;
use axum::{
    Extension, Json, Router,
    extract::{self, Query},
    routing::{get, post},
};
use chrono::{DateTime, SecondsFormat, Utc};
use collections::{HashMap, HashSet};
use reqwest::StatusCode;
use sea_orm::ActiveValue;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{str::FromStr, sync::Arc, time::Duration};
use stripe::{
    BillingPortalSession, CancellationDetailsReason, CreateBillingPortalSession,
    CreateBillingPortalSessionFlowData, CreateBillingPortalSessionFlowDataAfterCompletion,
    CreateBillingPortalSessionFlowDataAfterCompletionRedirect,
    CreateBillingPortalSessionFlowDataSubscriptionUpdateConfirm,
    CreateBillingPortalSessionFlowDataSubscriptionUpdateConfirmItems,
    CreateBillingPortalSessionFlowDataType, CustomerId, EventObject, EventType, ListEvents,
    PaymentMethod, Subscription, SubscriptionId, SubscriptionStatus,
};
use util::{ResultExt, maybe};
use zed_llm_client::LanguageModelProvider;

use crate::api::events::SnowflakeRow;
use crate::db::billing_subscription::{
    StripeCancellationReason, StripeSubscriptionStatus, SubscriptionKind,
};
use crate::llm::AGENT_EXTENDED_TRIAL_FEATURE_FLAG;
use crate::llm::db::subscription_usage_meter::{self, CompletionMode};
use crate::rpc::{ResultExt as _, Server};
use crate::stripe_client::{
    StripeCancellationDetailsReason, StripeClient, StripeCustomerId, StripeSubscription,
    StripeSubscriptionId, UpdateCustomerParams,
};
use crate::{AppState, Error, Result};
use crate::{db::UserId, llm::db::LlmDatabase};
use crate::{
    db::{
        BillingSubscriptionId, CreateBillingCustomerParams, CreateBillingSubscriptionParams,
        CreateProcessedStripeEventParams, UpdateBillingCustomerParams,
        UpdateBillingPreferencesParams, UpdateBillingSubscriptionParams, billing_customer,
    },
    stripe_billing::StripeBilling,
};

pub fn router() -> Router {
    Router::new()
        .route("/billing/preferences", put(update_billing_preferences))
        .route(
            "/billing/subscriptions",
            get(list_billing_subscriptions).post(create_billing_subscription),
        )
        .route(
            "/billing/subscriptions/manage",
            post(manage_billing_subscription),
        )
        .route(
            "/billing/subscriptions/sync",
            post(sync_billing_subscription),
        )
        .route("/billing/usage", get(get_current_usage))
}

#[derive(Debug, Serialize)]
struct BillingPreferencesResponse {
    trial_started_at: Option<String>,
    max_monthly_llm_usage_spending_in_cents: i32,
    model_request_overages_enabled: bool,
    model_request_overages_spend_limit_in_cents: i32,
}

#[derive(Debug, Deserialize)]
struct UpdateBillingPreferencesBody {
    github_user_id: i32,
    #[serde(default)]
    max_monthly_llm_usage_spending_in_cents: i32,
    #[serde(default)]
    model_request_overages_enabled: bool,
    #[serde(default)]
    model_request_overages_spend_limit_in_cents: i32,
}

async fn update_billing_preferences(
    Extension(app): Extension<Arc<AppState>>,
    Extension(rpc_server): Extension<Arc<crate::rpc::Server>>,
    extract::Json(body): extract::Json<UpdateBillingPreferencesBody>,
) -> Result<Json<BillingPreferencesResponse>> {
    let user = app
        .db
        .get_user_by_github_user_id(body.github_user_id)
        .await?
        .context("user not found")?;

    let billing_customer = app.db.get_billing_customer_by_user_id(user.id).await?;

    let max_monthly_llm_usage_spending_in_cents =
        body.max_monthly_llm_usage_spending_in_cents.max(0);
    let model_request_overages_spend_limit_in_cents =
        body.model_request_overages_spend_limit_in_cents.max(0);

    let billing_preferences =
        if let Some(_billing_preferences) = app.db.get_billing_preferences(user.id).await? {
            app.db
                .update_billing_preferences(
                    user.id,
                    &UpdateBillingPreferencesParams {
                        max_monthly_llm_usage_spending_in_cents: ActiveValue::set(
                            max_monthly_llm_usage_spending_in_cents,
                        ),
                        model_request_overages_enabled: ActiveValue::set(
                            body.model_request_overages_enabled,
                        ),
                        model_request_overages_spend_limit_in_cents: ActiveValue::set(
                            model_request_overages_spend_limit_in_cents,
                        ),
                    },
                )
                .await?
        } else {
            app.db
                .create_billing_preferences(
                    user.id,
                    &crate::db::CreateBillingPreferencesParams {
                        max_monthly_llm_usage_spending_in_cents,
                        model_request_overages_enabled: body.model_request_overages_enabled,
                        model_request_overages_spend_limit_in_cents,
                    },
                )
                .await?
        };

    SnowflakeRow::new(
        "Billing Preferences Updated",
        Some(user.metrics_id),
        user.admin,
        None,
        json!({
            "user_id": user.id,
            "model_request_overages_enabled": billing_preferences.model_request_overages_enabled,
            "model_request_overages_spend_limit_in_cents": billing_preferences.model_request_overages_spend_limit_in_cents,
            "max_monthly_llm_usage_spending_in_cents": billing_preferences.max_monthly_llm_usage_spending_in_cents,
        }),
    )
    .write(&app.kinesis_client, &app.config.kinesis_stream)
    .await
    .log_err();

    rpc_server.refresh_llm_tokens_for_user(user.id).await;

    Ok(Json(BillingPreferencesResponse {
        trial_started_at: billing_customer
            .and_then(|billing_customer| billing_customer.trial_started_at)
            .map(|trial_started_at| {
                trial_started_at
                    .and_utc()
                    .to_rfc3339_opts(SecondsFormat::Millis, true)
            }),
        max_monthly_llm_usage_spending_in_cents: billing_preferences
            .max_monthly_llm_usage_spending_in_cents,
        model_request_overages_enabled: billing_preferences.model_request_overages_enabled,
        model_request_overages_spend_limit_in_cents: billing_preferences
            .model_request_overages_spend_limit_in_cents,
    }))
}

#[derive(Debug, Deserialize)]
struct ListBillingSubscriptionsParams {
    github_user_id: i32,
}

#[derive(Debug, Serialize)]
struct BillingSubscriptionJson {
    id: BillingSubscriptionId,
    name: String,
    status: StripeSubscriptionStatus,
    period: Option<BillingSubscriptionPeriodJson>,
    trial_end_at: Option<String>,
    cancel_at: Option<String>,
    /// Whether this subscription can be canceled.
    is_cancelable: bool,
}

#[derive(Debug, Serialize)]
struct BillingSubscriptionPeriodJson {
    start_at: String,
    end_at: String,
}

#[derive(Debug, Serialize)]
struct ListBillingSubscriptionsResponse {
    subscriptions: Vec<BillingSubscriptionJson>,
}

async fn list_billing_subscriptions(
    Extension(app): Extension<Arc<AppState>>,
    Query(params): Query<ListBillingSubscriptionsParams>,
) -> Result<Json<ListBillingSubscriptionsResponse>> {
    let user = app
        .db
        .get_user_by_github_user_id(params.github_user_id)
        .await?
        .context("user not found")?;

    let subscriptions = app.db.get_billing_subscriptions(user.id).await?;

    Ok(Json(ListBillingSubscriptionsResponse {
        subscriptions: subscriptions
            .into_iter()
            .map(|subscription| BillingSubscriptionJson {
                id: subscription.id,
                name: match subscription.kind {
                    Some(SubscriptionKind::ZedPro) => "Zed Pro".to_string(),
                    Some(SubscriptionKind::ZedProTrial) => "Zed Pro (Trial)".to_string(),
                    Some(SubscriptionKind::ZedFree) => "Zed Free".to_string(),
                    None => "Zed LLM Usage".to_string(),
                },
                status: subscription.stripe_subscription_status,
                period: maybe!({
                    let start_at = subscription.current_period_start_at()?;
                    let end_at = subscription.current_period_end_at()?;

                    Some(BillingSubscriptionPeriodJson {
                        start_at: start_at.to_rfc3339_opts(SecondsFormat::Millis, true),
                        end_at: end_at.to_rfc3339_opts(SecondsFormat::Millis, true),
                    })
                }),
                trial_end_at: if subscription.kind == Some(SubscriptionKind::ZedProTrial) {
                    maybe!({
                        let end_at = subscription.stripe_current_period_end?;
                        let end_at = DateTime::from_timestamp(end_at, 0)?;

                        Some(end_at.to_rfc3339_opts(SecondsFormat::Millis, true))
                    })
                } else {
                    None
                },
                cancel_at: subscription.stripe_cancel_at.map(|cancel_at| {
                    cancel_at
                        .and_utc()
                        .to_rfc3339_opts(SecondsFormat::Millis, true)
                }),
                is_cancelable: subscription.kind != Some(SubscriptionKind::ZedFree)
                    && subscription.stripe_subscription_status.is_cancelable()
                    && subscription.stripe_cancel_at.is_none(),
            })
            .collect(),
    }))
}

#[derive(Debug, PartialEq, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProductCode {
    ZedPro,
    ZedProTrial,
}

#[derive(Debug, Deserialize)]
struct CreateBillingSubscriptionBody {
    github_user_id: i32,
    product: ProductCode,
}

#[derive(Debug, Serialize)]
struct CreateBillingSubscriptionResponse {
    checkout_session_url: String,
}

/// Initiates a Stripe Checkout session for creating a billing subscription.
async fn create_billing_subscription(
    Extension(app): Extension<Arc<AppState>>,
    extract::Json(body): extract::Json<CreateBillingSubscriptionBody>,
) -> Result<Json<CreateBillingSubscriptionResponse>> {
    let user = app
        .db
        .get_user_by_github_user_id(body.github_user_id)
        .await?
        .context("user not found")?;

    let Some(stripe_billing) = app.stripe_billing.clone() else {
        log::error!("failed to retrieve Stripe billing object");
        Err(Error::http(
            StatusCode::NOT_IMPLEMENTED,
            "not supported".into(),
        ))?
    };

    if let Some(existing_subscription) = app.db.get_active_billing_subscription(user.id).await? {
        let is_checkout_allowed = body.product == ProductCode::ZedProTrial
            && existing_subscription.kind == Some(SubscriptionKind::ZedFree);

        if !is_checkout_allowed {
            return Err(Error::http(
                StatusCode::CONFLICT,
                "user already has an active subscription".into(),
            ));
        }
    }

    let existing_billing_customer = app.db.get_billing_customer_by_user_id(user.id).await?;
    if let Some(existing_billing_customer) = &existing_billing_customer {
        if existing_billing_customer.has_overdue_invoices {
            return Err(Error::http(
                StatusCode::PAYMENT_REQUIRED,
                "user has overdue invoices".into(),
            ));
        }
    }

    let customer_id = if let Some(existing_customer) = &existing_billing_customer {
        let customer_id = StripeCustomerId(existing_customer.stripe_customer_id.clone().into());
        if let Some(email) = user.email_address.as_deref() {
            stripe_billing
                .client()
                .update_customer(&customer_id, UpdateCustomerParams { email: Some(email) })
                .await
                // Update of email address is best-effort - continue checkout even if it fails
                .context("error updating stripe customer email address")
                .log_err();
        }
        customer_id
    } else {
        stripe_billing
            .find_or_create_customer_by_email(user.email_address.as_deref())
            .await?
    };

    let success_url = format!(
        "{}/account?checkout_complete=1",
        app.config.zed_dot_dev_url()
    );

    let checkout_session_url = match body.product {
        ProductCode::ZedPro => {
            stripe_billing
                .checkout_with_zed_pro(&customer_id, &user.github_login, &success_url)
                .await?
        }
        ProductCode::ZedProTrial => {
            if let Some(existing_billing_customer) = &existing_billing_customer {
                if existing_billing_customer.trial_started_at.is_some() {
                    return Err(Error::http(
                        StatusCode::FORBIDDEN,
                        "user already used free trial".into(),
                    ));
                }
            }

            let feature_flags = app.db.get_user_flags(user.id).await?;

            stripe_billing
                .checkout_with_zed_pro_trial(
                    &customer_id,
                    &user.github_login,
                    feature_flags,
                    &success_url,
                )
                .await?
        }
    };

    Ok(Json(CreateBillingSubscriptionResponse {
        checkout_session_url,
    }))
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManageSubscriptionIntent {
    /// The user intends to manage their subscription.
    ///
    /// This will open the Stripe billing portal without putting the user in a specific flow.
    ManageSubscription,
    /// The user intends to update their payment method.
    UpdatePaymentMethod,
    /// The user intends to upgrade to Zed Pro.
    UpgradeToPro,
    /// The user intends to cancel their subscription.
    Cancel,
    /// The user intends to stop the cancellation of their subscription.
    StopCancellation,
}

#[derive(Debug, Deserialize)]
struct ManageBillingSubscriptionBody {
    github_user_id: i32,
    intent: ManageSubscriptionIntent,
    /// The ID of the subscription to manage.
    subscription_id: BillingSubscriptionId,
    redirect_to: Option<String>,
}

#[derive(Debug, Serialize)]
struct ManageBillingSubscriptionResponse {
    billing_portal_session_url: Option<String>,
}

/// Initiates a Stripe customer portal session for managing a billing subscription.
async fn manage_billing_subscription(
    Extension(app): Extension<Arc<AppState>>,
    extract::Json(body): extract::Json<ManageBillingSubscriptionBody>,
) -> Result<Json<ManageBillingSubscriptionResponse>> {
    let user = app
        .db
        .get_user_by_github_user_id(body.github_user_id)
        .await?
        .context("user not found")?;

    let Some(stripe_client) = app.real_stripe_client.clone() else {
        log::error!("failed to retrieve Stripe client");
        Err(Error::http(
            StatusCode::NOT_IMPLEMENTED,
            "not supported".into(),
        ))?
    };

    let Some(stripe_billing) = app.stripe_billing.clone() else {
        log::error!("failed to retrieve Stripe billing object");
        Err(Error::http(
            StatusCode::NOT_IMPLEMENTED,
            "not supported".into(),
        ))?
    };

    let customer = app
        .db
        .get_billing_customer_by_user_id(user.id)
        .await?
        .context("billing customer not found")?;
    let customer_id = CustomerId::from_str(&customer.stripe_customer_id)
        .context("failed to parse customer ID")?;

    let subscription = app
        .db
        .get_billing_subscription_by_id(body.subscription_id)
        .await?
        .context("subscription not found")?;
    let subscription_id = SubscriptionId::from_str(&subscription.stripe_subscription_id)
        .context("failed to parse subscription ID")?;

    if body.intent == ManageSubscriptionIntent::StopCancellation {
        let updated_stripe_subscription = Subscription::update(
            &stripe_client,
            &subscription_id,
            stripe::UpdateSubscription {
                cancel_at_period_end: Some(false),
                ..Default::default()
            },
        )
        .await?;

        app.db
            .update_billing_subscription(
                subscription.id,
                &UpdateBillingSubscriptionParams {
                    stripe_cancel_at: ActiveValue::set(
                        updated_stripe_subscription
                            .cancel_at
                            .and_then(|cancel_at| DateTime::from_timestamp(cancel_at, 0))
                            .map(|time| time.naive_utc()),
                    ),
                    ..Default::default()
                },
            )
            .await?;

        return Ok(Json(ManageBillingSubscriptionResponse {
            billing_portal_session_url: None,
        }));
    }

    let flow = match body.intent {
        ManageSubscriptionIntent::ManageSubscription => None,
        ManageSubscriptionIntent::UpgradeToPro => {
            let zed_pro_price_id: stripe::PriceId =
                stripe_billing.zed_pro_price_id().await?.try_into()?;
            let zed_free_price_id: stripe::PriceId =
                stripe_billing.zed_free_price_id().await?.try_into()?;

            let stripe_subscription =
                Subscription::retrieve(&stripe_client, &subscription_id, &[]).await?;

            let is_on_zed_pro_trial = stripe_subscription.status == SubscriptionStatus::Trialing
                && stripe_subscription.items.data.iter().any(|item| {
                    item.price
                        .as_ref()
                        .map_or(false, |price| price.id == zed_pro_price_id)
                });
            if is_on_zed_pro_trial {
                let payment_methods = PaymentMethod::list(
                    &stripe_client,
                    &stripe::ListPaymentMethods {
                        customer: Some(stripe_subscription.customer.id()),
                        ..Default::default()
                    },
                )
                .await?;

                let has_payment_method = !payment_methods.data.is_empty();
                if !has_payment_method {
                    return Err(Error::http(
                        StatusCode::BAD_REQUEST,
                        "missing payment method".into(),
                    ));
                }

                // If the user is already on a Zed Pro trial and wants to upgrade to Pro, we just need to end their trial early.
                Subscription::update(
                    &stripe_client,
                    &stripe_subscription.id,
                    stripe::UpdateSubscription {
                        trial_end: Some(stripe::Scheduled::now()),
                        ..Default::default()
                    },
                )
                .await?;

                return Ok(Json(ManageBillingSubscriptionResponse {
                    billing_portal_session_url: None,
                }));
            }

            let subscription_item_to_update = stripe_subscription
                .items
                .data
                .iter()
                .find_map(|item| {
                    let price = item.price.as_ref()?;

                    if price.id == zed_free_price_id {
                        Some(item.id.clone())
                    } else {
                        None
                    }
                })
                .context("No subscription item to update")?;

            Some(CreateBillingPortalSessionFlowData {
                type_: CreateBillingPortalSessionFlowDataType::SubscriptionUpdateConfirm,
                subscription_update_confirm: Some(
                    CreateBillingPortalSessionFlowDataSubscriptionUpdateConfirm {
                        subscription: subscription.stripe_subscription_id,
                        items: vec![
                            CreateBillingPortalSessionFlowDataSubscriptionUpdateConfirmItems {
                                id: subscription_item_to_update.to_string(),
                                price: Some(zed_pro_price_id.to_string()),
                                quantity: Some(1),
                            },
                        ],
                        discounts: None,
                    },
                ),
                ..Default::default()
            })
        }
        ManageSubscriptionIntent::UpdatePaymentMethod => Some(CreateBillingPortalSessionFlowData {
            type_: CreateBillingPortalSessionFlowDataType::PaymentMethodUpdate,
            after_completion: Some(CreateBillingPortalSessionFlowDataAfterCompletion {
                type_: stripe::CreateBillingPortalSessionFlowDataAfterCompletionType::Redirect,
                redirect: Some(CreateBillingPortalSessionFlowDataAfterCompletionRedirect {
                    return_url: format!(
                        "{}{path}",
                        app.config.zed_dot_dev_url(),
                        path = body.redirect_to.unwrap_or_else(|| "/account".to_string())
                    ),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ManageSubscriptionIntent::Cancel => {
            if subscription.kind == Some(SubscriptionKind::ZedFree) {
                return Err(Error::http(
                    StatusCode::BAD_REQUEST,
                    "free subscription cannot be canceled".into(),
                ));
            }

            Some(CreateBillingPortalSessionFlowData {
                type_: CreateBillingPortalSessionFlowDataType::SubscriptionCancel,
                after_completion: Some(CreateBillingPortalSessionFlowDataAfterCompletion {
                    type_: stripe::CreateBillingPortalSessionFlowDataAfterCompletionType::Redirect,
                    redirect: Some(CreateBillingPortalSessionFlowDataAfterCompletionRedirect {
                        return_url: format!("{}/account", app.config.zed_dot_dev_url()),
                    }),
                    ..Default::default()
                }),
                subscription_cancel: Some(
                    stripe::CreateBillingPortalSessionFlowDataSubscriptionCancel {
                        subscription: subscription.stripe_subscription_id,
                        retention: None,
                    },
                ),
                ..Default::default()
            })
        }
        ManageSubscriptionIntent::StopCancellation => unreachable!(),
    };

    let mut params = CreateBillingPortalSession::new(customer_id);
    params.flow_data = flow;
    let return_url = format!("{}/account", app.config.zed_dot_dev_url());
    params.return_url = Some(&return_url);

    let session = BillingPortalSession::create(&stripe_client, params).await?;

    Ok(Json(ManageBillingSubscriptionResponse {
        billing_portal_session_url: Some(session.url),
    }))
}

#[derive(Debug, Deserialize)]
struct SyncBillingSubscriptionBody {
    github_user_id: i32,
}

#[derive(Debug, Serialize)]
struct SyncBillingSubscriptionResponse {
    stripe_customer_id: String,
}

async fn sync_billing_subscription(
    Extension(app): Extension<Arc<AppState>>,
    extract::Json(body): extract::Json<SyncBillingSubscriptionBody>,
) -> Result<Json<SyncBillingSubscriptionResponse>> {
    let Some(stripe_client) = app.stripe_client.clone() else {
        log::error!("failed to retrieve Stripe client");
        Err(Error::http(
            StatusCode::NOT_IMPLEMENTED,
            "not supported".into(),
        ))?
    };

    let user = app
        .db
        .get_user_by_github_user_id(body.github_user_id)
        .await?
        .context("user not found")?;

    let billing_customer = app
        .db
        .get_billing_customer_by_user_id(user.id)
        .await?
        .context("billing customer not found")?;
    let stripe_customer_id = StripeCustomerId(billing_customer.stripe_customer_id.clone().into());

    let subscriptions = stripe_client
        .list_subscriptions_for_customer(&stripe_customer_id)
        .await?;

    for subscription in subscriptions {
        let subscription_id = subscription.id.clone();

        sync_subscription(&app, &stripe_client, subscription)
            .await
            .with_context(|| {
                format!(
                    "failed to sync subscription {subscription_id} for user {}",
                    user.id,
                )
            })?;
    }

    Ok(Json(SyncBillingSubscriptionResponse {
        stripe_customer_id: billing_customer.stripe_customer_id.clone(),
    }))
}

/// The amount of time we wait in between each poll of Stripe events.
///
/// This value should strike a balance between:
///   1. Being short enough that we update quickly when something in Stripe changes
///   2. Being long enough that we don't eat into our rate limits.
///
/// As a point of reference, the Sequin folks say they have this at **500ms**:
///
/// > We poll the Stripe /events endpoint every 500ms per account
/// >
/// > — https://blog.sequinstream.com/events-not-webhooks/
const POLL_EVENTS_INTERVAL: Duration = Duration::from_secs(5);

/// The maximum number of events to return per page.
///
/// We set this to 100 (the max) so we have to make fewer requests to Stripe.
///
/// > Limit can range between 1 and 100, and the default is 10.
const EVENTS_LIMIT_PER_PAGE: u64 = 100;

/// The number of pages consisting entirely of already-processed events that we
/// will see before we stop retrieving events.
///
/// This is used to prevent over-fetching the Stripe events API for events we've
/// already seen and processed.
const NUMBER_OF_ALREADY_PROCESSED_PAGES_BEFORE_WE_STOP: usize = 4;

/// Polls the Stripe events API periodically to reconcile the records in our
/// database with the data in Stripe.
pub fn poll_stripe_events_periodically(app: Arc<AppState>, rpc_server: Arc<Server>) {
    let Some(real_stripe_client) = app.real_stripe_client.clone() else {
        log::warn!("failed to retrieve Stripe client");
        return;
    };
    let Some(stripe_client) = app.stripe_client.clone() else {
        log::warn!("failed to retrieve Stripe client");
        return;
    };

    let executor = app.executor.clone();
    executor.spawn_detached({
        let executor = executor.clone();
        async move {
            loop {
                poll_stripe_events(&app, &rpc_server, &stripe_client, &real_stripe_client)
                    .await
                    .log_err();

                executor.sleep(POLL_EVENTS_INTERVAL).await;
            }
        }
    });
}

async fn poll_stripe_events(
    app: &Arc<AppState>,
    rpc_server: &Arc<Server>,
    stripe_client: &Arc<dyn StripeClient>,
    real_stripe_client: &stripe::Client,
) -> anyhow::Result<()> {
    fn event_type_to_string(event_type: EventType) -> String {
        // Calling `to_string` on `stripe::EventType` members gives us a quoted string,
        // so we need to unquote it.
        event_type.to_string().trim_matches('"').to_string()
    }

    let event_types = [
        EventType::CustomerCreated,
        EventType::CustomerUpdated,
        EventType::CustomerSubscriptionCreated,
        EventType::CustomerSubscriptionUpdated,
        EventType::CustomerSubscriptionPaused,
        EventType::CustomerSubscriptionResumed,
        EventType::CustomerSubscriptionDeleted,
    ]
    .into_iter()
    .map(event_type_to_string)
    .collect::<Vec<_>>();

    let mut pages_of_already_processed_events = 0;
    let mut unprocessed_events = Vec::new();

    log::info!(
        "Stripe events: starting retrieval for {}",
        event_types.join(", ")
    );
    let mut params = ListEvents::new();
    params.types = Some(event_types.clone());
    params.limit = Some(EVENTS_LIMIT_PER_PAGE);

    let mut event_pages = stripe::Event::list(&real_stripe_client, &params)
        .await?
        .paginate(params);

    loop {
        let processed_event_ids = {
            let event_ids = event_pages
                .page
                .data
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>();
            app.db
                .get_processed_stripe_events_by_event_ids(&event_ids)
                .await?
                .into_iter()
                .map(|event| event.stripe_event_id)
                .collect::<Vec<_>>()
        };

        let mut processed_events_in_page = 0;
        let events_in_page = event_pages.page.data.len();
        for event in &event_pages.page.data {
            if processed_event_ids.contains(&event.id.to_string()) {
                processed_events_in_page += 1;
                log::debug!("Stripe events: already processed '{}', skipping", event.id);
            } else {
                unprocessed_events.push(event.clone());
            }
        }

        if processed_events_in_page == events_in_page {
            pages_of_already_processed_events += 1;
        }

        if event_pages.page.has_more {
            if pages_of_already_processed_events >= NUMBER_OF_ALREADY_PROCESSED_PAGES_BEFORE_WE_STOP
            {
                log::info!(
                    "Stripe events: stopping, saw {pages_of_already_processed_events} pages of already-processed events"
                );
                break;
            } else {
                log::info!("Stripe events: retrieving next page");
                event_pages = event_pages.next(&real_stripe_client).await?;
            }
        } else {
            break;
        }
    }

    log::info!("Stripe events: unprocessed {}", unprocessed_events.len());

    // Sort all of the unprocessed events in ascending order, so we can handle them in the order they occurred.
    unprocessed_events.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));

    for event in unprocessed_events {
        let event_id = event.id.clone();
        let processed_event_params = CreateProcessedStripeEventParams {
            stripe_event_id: event.id.to_string(),
            stripe_event_type: event_type_to_string(event.type_),
            stripe_event_created_timestamp: event.created,
        };

        // If the event has happened too far in the past, we don't want to
        // process it and risk overwriting other more-recent updates.
        //
        // 1 day was chosen arbitrarily. This could be made longer or shorter.
        let one_day = Duration::from_secs(24 * 60 * 60);
        let a_day_ago = Utc::now() - one_day;
        if a_day_ago.timestamp() > event.created {
            log::info!(
                "Stripe events: event '{}' is more than {one_day:?} old, marking as processed",
                event_id
            );
            app.db
                .create_processed_stripe_event(&processed_event_params)
                .await?;

            continue;
        }

        let process_result = match event.type_ {
            EventType::CustomerCreated | EventType::CustomerUpdated => {
                handle_customer_event(app, real_stripe_client, event).await
            }
            EventType::CustomerSubscriptionCreated
            | EventType::CustomerSubscriptionUpdated
            | EventType::CustomerSubscriptionPaused
            | EventType::CustomerSubscriptionResumed
            | EventType::CustomerSubscriptionDeleted => {
                handle_customer_subscription_event(app, rpc_server, stripe_client, event).await
            }
            _ => Ok(()),
        };

        if let Some(()) = process_result
            .with_context(|| format!("failed to process event {event_id} successfully"))
            .log_err()
        {
            app.db
                .create_processed_stripe_event(&processed_event_params)
                .await?;
        }
    }

    Ok(())
}

async fn handle_customer_event(
    app: &Arc<AppState>,
    _stripe_client: &stripe::Client,
    event: stripe::Event,
) -> anyhow::Result<()> {
    let EventObject::Customer(customer) = event.data.object else {
        bail!("unexpected event payload for {}", event.id);
    };

    log::info!("handling Stripe {} event: {}", event.type_, event.id);

    let Some(email) = customer.email else {
        log::info!("Stripe customer has no email: skipping");
        return Ok(());
    };

    let Some(user) = app.db.get_user_by_email(&email).await? else {
        log::info!("no user found for email: skipping");
        return Ok(());
    };

    if let Some(existing_customer) = app
        .db
        .get_billing_customer_by_stripe_customer_id(&customer.id)
        .await?
    {
        app.db
            .update_billing_customer(
                existing_customer.id,
                &UpdateBillingCustomerParams {
                    // For now we just leave the information as-is, as it is not
                    // likely to change.
                    ..Default::default()
                },
            )
            .await?;
    } else {
        app.db
            .create_billing_customer(&CreateBillingCustomerParams {
                user_id: user.id,
                stripe_customer_id: customer.id.to_string(),
            })
            .await?;
    }

    Ok(())
}

async fn sync_subscription(
    app: &Arc<AppState>,
    stripe_client: &Arc<dyn StripeClient>,
    subscription: StripeSubscription,
) -> anyhow::Result<billing_customer::Model> {
    let subscription_kind = if let Some(stripe_billing) = &app.stripe_billing {
        stripe_billing
            .determine_subscription_kind(&subscription)
            .await
    } else {
        None
    };

    let billing_customer =
        find_or_create_billing_customer(app, stripe_client.as_ref(), &subscription.customer)
            .await?
            .context("billing customer not found")?;

    if let Some(SubscriptionKind::ZedProTrial) = subscription_kind {
        if subscription.status == SubscriptionStatus::Trialing {
            let current_period_start =
                DateTime::from_timestamp(subscription.current_period_start, 0)
                    .context("No trial subscription period start")?;

            app.db
                .update_billing_customer(
                    billing_customer.id,
                    &UpdateBillingCustomerParams {
                        trial_started_at: ActiveValue::set(Some(current_period_start.naive_utc())),
                        ..Default::default()
                    },
                )
                .await?;
        }
    }

    let was_canceled_due_to_payment_failure = subscription.status == SubscriptionStatus::Canceled
        && subscription
            .cancellation_details
            .as_ref()
            .and_then(|details| details.reason)
            .map_or(false, |reason| {
                reason == StripeCancellationDetailsReason::PaymentFailed
            });

    if was_canceled_due_to_payment_failure {
        app.db
            .update_billing_customer(
                billing_customer.id,
                &UpdateBillingCustomerParams {
                    has_overdue_invoices: ActiveValue::set(true),
                    ..Default::default()
                },
            )
            .await?;
    }

    if let Some(existing_subscription) = app
        .db
        .get_billing_subscription_by_stripe_subscription_id(subscription.id.0.as_ref())
        .await?
    {
        app.db
            .update_billing_subscription(
                existing_subscription.id,
                &UpdateBillingSubscriptionParams {
                    billing_customer_id: ActiveValue::set(billing_customer.id),
                    kind: ActiveValue::set(subscription_kind),
                    stripe_subscription_id: ActiveValue::set(subscription.id.to_string()),
                    stripe_subscription_status: ActiveValue::set(subscription.status.into()),
                    stripe_cancel_at: ActiveValue::set(
                        subscription
                            .cancel_at
                            .and_then(|cancel_at| DateTime::from_timestamp(cancel_at, 0))
                            .map(|time| time.naive_utc()),
                    ),
                    stripe_cancellation_reason: ActiveValue::set(
                        subscription
                            .cancellation_details
                            .and_then(|details| details.reason)
                            .map(|reason| reason.into()),
                    ),
                    stripe_current_period_start: ActiveValue::set(Some(
                        subscription.current_period_start,
                    )),
                    stripe_current_period_end: ActiveValue::set(Some(
                        subscription.current_period_end,
                    )),
                },
            )
            .await?;
    } else {
        if let Some(existing_subscription) = app
            .db
            .get_active_billing_subscription(billing_customer.user_id)
            .await?
        {
            if existing_subscription.kind == Some(SubscriptionKind::ZedFree)
                && subscription_kind == Some(SubscriptionKind::ZedProTrial)
            {
                let stripe_subscription_id = StripeSubscriptionId(
                    existing_subscription.stripe_subscription_id.clone().into(),
                );

                stripe_client
                    .cancel_subscription(&stripe_subscription_id)
                    .await?;
            } else {
                // If the user already has an active billing subscription, ignore the
                // event and return an `Ok` to signal that it was processed
                // successfully.
                //
                // There is the possibility that this could cause us to not create a
                // subscription in the following scenario:
                //
                //   1. User has an active subscription A
                //   2. User cancels subscription A
                //   3. User creates a new subscription B
                //   4. We process the new subscription B before the cancellation of subscription A
                //   5. User ends up with no subscriptions
                //
                // In theory this situation shouldn't arise as we try to process the events in the order they occur.

                log::info!(
                    "user {user_id} already has an active subscription, skipping creation of subscription {subscription_id}",
                    user_id = billing_customer.user_id,
                    subscription_id = subscription.id
                );
                return Ok(billing_customer);
            }
        }

        app.db
            .create_billing_subscription(&CreateBillingSubscriptionParams {
                billing_customer_id: billing_customer.id,
                kind: subscription_kind,
                stripe_subscription_id: subscription.id.to_string(),
                stripe_subscription_status: subscription.status.into(),
                stripe_cancellation_reason: subscription
                    .cancellation_details
                    .and_then(|details| details.reason)
                    .map(|reason| reason.into()),
                stripe_current_period_start: Some(subscription.current_period_start),
                stripe_current_period_end: Some(subscription.current_period_end),
            })
            .await?;
    }

    if let Some(stripe_billing) = app.stripe_billing.as_ref() {
        if subscription.status == SubscriptionStatus::Canceled
            || subscription.status == SubscriptionStatus::Paused
        {
            let already_has_active_billing_subscription = app
                .db
                .has_active_billing_subscription(billing_customer.user_id)
                .await?;
            if !already_has_active_billing_subscription {
                let stripe_customer_id =
                    StripeCustomerId(billing_customer.stripe_customer_id.clone().into());

                stripe_billing
                    .subscribe_to_zed_free(stripe_customer_id)
                    .await?;
            }
        }
    }

    Ok(billing_customer)
}

async fn handle_customer_subscription_event(
    app: &Arc<AppState>,
    rpc_server: &Arc<Server>,
    stripe_client: &Arc<dyn StripeClient>,
    event: stripe::Event,
) -> anyhow::Result<()> {
    let EventObject::Subscription(subscription) = event.data.object else {
        bail!("unexpected event payload for {}", event.id);
    };

    log::info!("handling Stripe {} event: {}", event.type_, event.id);

    let billing_customer = sync_subscription(app, stripe_client, subscription.into()).await?;

    // When the user's subscription changes, push down any changes to their plan.
    rpc_server
        .update_plan_for_user(billing_customer.user_id)
        .await
        .trace_err();

    // When the user's subscription changes, we want to refresh their LLM tokens
    // to either grant/revoke access.
    rpc_server
        .refresh_llm_tokens_for_user(billing_customer.user_id)
        .await;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct GetCurrentUsageParams {
    github_user_id: i32,
}

#[derive(Debug, Serialize)]
struct UsageCounts {
    pub used: i32,
    pub limit: Option<i32>,
    pub remaining: Option<i32>,
}

#[derive(Debug, Serialize)]
struct ModelRequestUsage {
    pub model: String,
    pub mode: CompletionMode,
    pub requests: i32,
}

#[derive(Debug, Serialize)]
struct CurrentUsage {
    pub model_requests: UsageCounts,
    pub model_request_usage: Vec<ModelRequestUsage>,
    pub edit_predictions: UsageCounts,
}

#[derive(Debug, Default, Serialize)]
struct GetCurrentUsageResponse {
    pub plan: String,
    pub current_usage: Option<CurrentUsage>,
}

async fn get_current_usage(
    Extension(app): Extension<Arc<AppState>>,
    Query(params): Query<GetCurrentUsageParams>,
) -> Result<Json<GetCurrentUsageResponse>> {
    let user = app
        .db
        .get_user_by_github_user_id(params.github_user_id)
        .await?
        .context("user not found")?;

    let feature_flags = app.db.get_user_flags(user.id).await?;
    let has_extended_trial = feature_flags
        .iter()
        .any(|flag| flag == AGENT_EXTENDED_TRIAL_FEATURE_FLAG);

    let Some(llm_db) = app.llm_db.clone() else {
        return Err(Error::http(
            StatusCode::NOT_IMPLEMENTED,
            "LLM database not available".into(),
        ));
    };

    let Some(subscription) = app.db.get_active_billing_subscription(user.id).await? else {
        return Ok(Json(GetCurrentUsageResponse::default()));
    };

    let subscription_period = maybe!({
        let period_start_at = subscription.current_period_start_at()?;
        let period_end_at = subscription.current_period_end_at()?;

        Some((period_start_at, period_end_at))
    });

    let Some((period_start_at, period_end_at)) = subscription_period else {
        return Ok(Json(GetCurrentUsageResponse::default()));
    };

    let usage = llm_db
        .get_subscription_usage_for_period(user.id, period_start_at, period_end_at)
        .await?;

    let plan = subscription
        .kind
        .map(Into::into)
        .unwrap_or(zed_llm_client::Plan::ZedFree);

    let model_requests_limit = match plan.model_requests_limit() {
        zed_llm_client::UsageLimit::Limited(limit) => {
            let limit = if plan == zed_llm_client::Plan::ZedProTrial && has_extended_trial {
                1_000
            } else {
                limit
            };

            Some(limit)
        }
        zed_llm_client::UsageLimit::Unlimited => None,
    };

    let edit_predictions_limit = match plan.edit_predictions_limit() {
        zed_llm_client::UsageLimit::Limited(limit) => Some(limit),
        zed_llm_client::UsageLimit::Unlimited => None,
    };

    let Some(usage) = usage else {
        return Ok(Json(GetCurrentUsageResponse {
            plan: plan.as_str().to_string(),
            current_usage: Some(CurrentUsage {
                model_requests: UsageCounts {
                    used: 0,
                    limit: model_requests_limit,
                    remaining: model_requests_limit,
                },
                model_request_usage: Vec::new(),
                edit_predictions: UsageCounts {
                    used: 0,
                    limit: edit_predictions_limit,
                    remaining: edit_predictions_limit,
                },
            }),
        }));
    };

    let subscription_usage_meters = llm_db
        .get_current_subscription_usage_meters_for_user(user.id, Utc::now())
        .await?;

    let model_request_usage = subscription_usage_meters
        .into_iter()
        .filter_map(|(usage_meter, _usage)| {
            let model = llm_db.model_by_id(usage_meter.model_id).ok()?;

            Some(ModelRequestUsage {
                model: model.name.clone(),
                mode: usage_meter.mode,
                requests: usage_meter.requests,
            })
        })
        .collect::<Vec<_>>();

    Ok(Json(GetCurrentUsageResponse {
        plan: plan.as_str().to_string(),
        current_usage: Some(CurrentUsage {
            model_requests: UsageCounts {
                used: usage.model_requests,
                limit: model_requests_limit,
                remaining: model_requests_limit.map(|limit| (limit - usage.model_requests).max(0)),
            },
            model_request_usage,
            edit_predictions: UsageCounts {
                used: usage.edit_predictions,
                limit: edit_predictions_limit,
                remaining: edit_predictions_limit
                    .map(|limit| (limit - usage.edit_predictions).max(0)),
            },
        }),
    }))
}

impl From<SubscriptionStatus> for StripeSubscriptionStatus {
    fn from(value: SubscriptionStatus) -> Self {
        match value {
            SubscriptionStatus::Incomplete => Self::Incomplete,
            SubscriptionStatus::IncompleteExpired => Self::IncompleteExpired,
            SubscriptionStatus::Trialing => Self::Trialing,
            SubscriptionStatus::Active => Self::Active,
            SubscriptionStatus::PastDue => Self::PastDue,
            SubscriptionStatus::Canceled => Self::Canceled,
            SubscriptionStatus::Unpaid => Self::Unpaid,
            SubscriptionStatus::Paused => Self::Paused,
        }
    }
}

impl From<CancellationDetailsReason> for StripeCancellationReason {
    fn from(value: CancellationDetailsReason) -> Self {
        match value {
            CancellationDetailsReason::CancellationRequested => Self::CancellationRequested,
            CancellationDetailsReason::PaymentDisputed => Self::PaymentDisputed,
            CancellationDetailsReason::PaymentFailed => Self::PaymentFailed,
        }
    }
}

/// Finds or creates a billing customer using the provided customer.
pub async fn find_or_create_billing_customer(
    app: &Arc<AppState>,
    stripe_client: &dyn StripeClient,
    customer_id: &StripeCustomerId,
) -> anyhow::Result<Option<billing_customer::Model>> {
    // If we already have a billing customer record associated with the Stripe customer,
    // there's nothing more we need to do.
    if let Some(billing_customer) = app
        .db
        .get_billing_customer_by_stripe_customer_id(customer_id.0.as_ref())
        .await?
    {
        return Ok(Some(billing_customer));
    }

    let customer = stripe_client.get_customer(customer_id).await?;

    let Some(email) = customer.email else {
        return Ok(None);
    };

    let Some(user) = app.db.get_user_by_email(&email).await? else {
        return Ok(None);
    };

    let billing_customer = app
        .db
        .create_billing_customer(&CreateBillingCustomerParams {
            user_id: user.id,
            stripe_customer_id: customer.id.to_string(),
        })
        .await?;

    Ok(Some(billing_customer))
}

const SYNC_LLM_REQUEST_USAGE_WITH_STRIPE_INTERVAL: Duration = Duration::from_secs(60);

pub fn sync_llm_request_usage_with_stripe_periodically(app: Arc<AppState>) {
    let Some(stripe_billing) = app.stripe_billing.clone() else {
        log::warn!("failed to retrieve Stripe billing object");
        return;
    };
    let Some(llm_db) = app.llm_db.clone() else {
        log::warn!("failed to retrieve LLM database");
        return;
    };

    let executor = app.executor.clone();
    executor.spawn_detached({
        let executor = executor.clone();
        async move {
            loop {
                sync_model_request_usage_with_stripe(&app, &llm_db, &stripe_billing)
                    .await
                    .context("failed to sync LLM request usage to Stripe")
                    .trace_err();
                executor
                    .sleep(SYNC_LLM_REQUEST_USAGE_WITH_STRIPE_INTERVAL)
                    .await;
            }
        }
    });
}

async fn sync_model_request_usage_with_stripe(
    app: &Arc<AppState>,
    llm_db: &Arc<LlmDatabase>,
    stripe_billing: &Arc<StripeBilling>,
) -> anyhow::Result<()> {
    log::info!("Stripe usage sync: Starting");
    let started_at = Utc::now();

    let staff_users = app.db.get_staff_users().await?;
    let staff_user_ids = staff_users
        .iter()
        .map(|user| user.id)
        .collect::<HashSet<UserId>>();

    let usage_meters = llm_db
        .get_current_subscription_usage_meters(Utc::now())
        .await?;
    let mut usage_meters_by_user_id =
        HashMap::<UserId, Vec<subscription_usage_meter::Model>>::default();
    for (usage_meter, usage) in usage_meters {
        let meters = usage_meters_by_user_id.entry(usage.user_id).or_default();
        meters.push(usage_meter);
    }

    log::info!("Stripe usage sync: Retrieving Zed Pro subscriptions");
    let get_zed_pro_subscriptions_started_at = Utc::now();
    let billing_subscriptions = app.db.get_active_zed_pro_billing_subscriptions().await?;
    log::info!(
        "Stripe usage sync: Retrieved {} Zed Pro subscriptions in {}",
        billing_subscriptions.len(),
        Utc::now() - get_zed_pro_subscriptions_started_at
    );

    let claude_sonnet_4 = stripe_billing
        .find_price_by_lookup_key("claude-sonnet-4-requests")
        .await?;
    let claude_sonnet_4_max = stripe_billing
        .find_price_by_lookup_key("claude-sonnet-4-requests-max")
        .await?;
    let claude_opus_4 = stripe_billing
        .find_price_by_lookup_key("claude-opus-4-requests")
        .await?;
    let claude_opus_4_max = stripe_billing
        .find_price_by_lookup_key("claude-opus-4-requests-max")
        .await?;
    let claude_3_5_sonnet = stripe_billing
        .find_price_by_lookup_key("claude-3-5-sonnet-requests")
        .await?;
    let claude_3_7_sonnet = stripe_billing
        .find_price_by_lookup_key("claude-3-7-sonnet-requests")
        .await?;
    let claude_3_7_sonnet_max = stripe_billing
        .find_price_by_lookup_key("claude-3-7-sonnet-requests-max")
        .await?;

    let model_mode_combinations = [
        ("claude-opus-4", CompletionMode::Max),
        ("claude-opus-4", CompletionMode::Normal),
        ("claude-sonnet-4", CompletionMode::Max),
        ("claude-sonnet-4", CompletionMode::Normal),
        ("claude-3-7-sonnet", CompletionMode::Max),
        ("claude-3-7-sonnet", CompletionMode::Normal),
        ("claude-3-5-sonnet", CompletionMode::Normal),
    ];

    let billing_subscription_count = billing_subscriptions.len();

    log::info!("Stripe usage sync: Syncing {billing_subscription_count} Zed Pro subscriptions");

    for (user_id, (billing_customer, billing_subscription)) in billing_subscriptions {
        maybe!(async {
            if staff_user_ids.contains(&user_id) {
                return anyhow::Ok(());
            }

            let stripe_customer_id =
                StripeCustomerId(billing_customer.stripe_customer_id.clone().into());
            let stripe_subscription_id =
                StripeSubscriptionId(billing_subscription.stripe_subscription_id.clone().into());

            let usage_meters = usage_meters_by_user_id.get(&user_id);

            for (model, mode) in &model_mode_combinations {
                let Ok(model) =
                    llm_db.model(LanguageModelProvider::Anthropic, model)
                else {
                    log::warn!("Failed to load model for user {user_id}: {model}");
                    continue;
                };

                let (price, meter_event_name) = match model.name.as_str() {
                    "claude-opus-4" => match mode {
                        CompletionMode::Normal => (&claude_opus_4, "claude_opus_4/requests"),
                        CompletionMode::Max => (&claude_opus_4_max, "claude_opus_4/requests/max"),
                    },
                    "claude-sonnet-4" => match mode {
                        CompletionMode::Normal => (&claude_sonnet_4, "claude_sonnet_4/requests"),
                        CompletionMode::Max => {
                            (&claude_sonnet_4_max, "claude_sonnet_4/requests/max")
                        }
                    },
                    "claude-3-5-sonnet" => (&claude_3_5_sonnet, "claude_3_5_sonnet/requests"),
                    "claude-3-7-sonnet" => match mode {
                        CompletionMode::Normal => {
                            (&claude_3_7_sonnet, "claude_3_7_sonnet/requests")
                        }
                        CompletionMode::Max => {
                            (&claude_3_7_sonnet_max, "claude_3_7_sonnet/requests/max")
                        }
                    },
                    model_name => {
                        bail!("Attempted to sync usage meter for unsupported model: {model_name:?}")
                    }
                };

                let model_requests = usage_meters
                    .and_then(|usage_meters| {
                        usage_meters
                            .iter()
                            .find(|meter| meter.model_id == model.id && meter.mode == *mode)
                    })
                    .map(|usage_meter| usage_meter.requests)
                    .unwrap_or(0);

                if model_requests > 0 {
                    stripe_billing
                        .subscribe_to_price(&stripe_subscription_id, price)
                        .await?;
                }

                stripe_billing
                    .bill_model_request_usage(&stripe_customer_id, meter_event_name, model_requests)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to bill model request usage of {model_requests} for {stripe_customer_id}: {meter_event_name}",
                        )
                    })?;
            }

            Ok(())
        })
        .await
        .log_err();
    }

    log::info!(
        "Stripe usage sync: Synced {billing_subscription_count} Zed Pro subscriptions in {}",
        Utc::now() - started_at
    );

    Ok(())
}
