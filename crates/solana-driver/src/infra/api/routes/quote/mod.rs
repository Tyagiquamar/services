//! The quote route: what one order would trade for right now.

pub mod dto;

use {
    crate::{
        domain::{
            auction::{self, Auction, Order},
            order_uid::OrderUid,
            slot::Slot,
        },
        infra::api::{
            LoggingJson,
            State,
            error::{Error as ApiError, Kind},
        },
    },
    axum::{Json, http::StatusCode},
    cow_settlement_interface::data::intent::ENCODED_NATIVE_SOL_TRANSFER,
    solana_sdk::pubkey::Pubkey,
    spl_token_interface::native_mint,
    tracing::Instrument,
};

/// Handle `POST /quote`: solve a one-order auction and answer with the
/// executed amounts of its best settleable route, the network fee of
/// settling them, and whether the settlement passed simulation.
pub(crate) async fn quote(
    state: axum::extract::State<State>,
    LoggingJson(request): LoggingJson<dto::QuoteRequest>,
) -> Result<Json<dto::QuoteResponse>, (StatusCode, Json<ApiError>)> {
    // wSOL and native SOL count as the same token.
    let unwraps_sol =
        request.sell_token == native_mint::ID && request.buy_token == ENCODED_NATIVE_SOL_TRANSFER;
    if request.sell_token == request.buy_token || unwraps_sol {
        return Err(Kind::QuoteSameTokens.into());
    }
    let side = auction::Side::from(request.kind);
    let auction = quote_auction(&request, side);

    let competition = state.competition();
    let span = tracing::info_span!("/quote", solver = %competition.solver_name());
    let Some(quote) = competition.quote(auction).instrument(span.clone()).await? else {
        return Err(Kind::QuotingFailed.into());
    };
    let network_fee = competition
        .network_fee(&quote.solution)
        .instrument(span)
        .await?;

    Ok(Json(dto::QuoteResponse {
        sell_amount: quote.trade.executed_sell,
        buy_amount: quote.trade.executed_buy,
        solver: quote.solution.solver,
        network_fee,
        verified: quote.verified,
    }))
}

/// The single-order auction a quote is solved in. Everything beyond the pair
/// and the amount is a placeholder: the verification settles an order of the
/// executed amounts in its place, see `domain::quote`.
fn quote_auction(request: &dto::QuoteRequest, side: auction::Side) -> Auction {
    let (sell_amount, buy_amount) = match side {
        // The counter amount is the quote's answer, so its limit stays
        // unconstrained: zero to buy for a sell order, max to sell for a
        // buy order.
        auction::Side::Sell => (request.amount, 0),
        auction::Side::Buy => (u64::MAX, request.amount),
    };
    Auction {
        id: None,
        orders: vec![Order {
            uid: OrderUid([0; 32]),
            owner: Pubkey::default(),
            sell_token: request.sell_token,
            buy_token: request.buy_token,
            sell_token_account: Pubkey::default(),
            buy_token_account: Pubkey::default(),
            sell_amount,
            buy_amount,
            valid_to: u32::MAX,
            side,
            partially_fillable: false,
            order_pda: Pubkey::default(),
            app_data: [0; 32],
            executed: 0,
            sell_balance: None,
        }],
        deadline_slot: Slot(0),
        deadline: request.deadline,
        creations: std::collections::HashMap::new(),
        native_prices: std::collections::HashMap::new(),
    }
}
