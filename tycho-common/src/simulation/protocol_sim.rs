use std::{any::Any, collections::HashMap, fmt};

use num_bigint::BigUint;

use crate::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::IndicativelyPriced,
    },
    Bytes,
};

#[derive(Default)]
pub struct Balances {
    pub component_balances: HashMap<String, HashMap<Bytes, Bytes>>,
    pub account_balances: HashMap<Bytes, HashMap<Bytes, Bytes>>,
}

/// GetAmountOutResult struct represents the result of getting the amount out of a trading pair
///
/// # Fields
///
/// * `amount`: BigUint, the amount of the trading pair
/// * `gas`: BigUint, the gas of the trading pair
#[derive(Debug)]
pub struct GetAmountOutResult {
    pub amount: BigUint,
    pub gas: BigUint,
    pub new_state: Box<dyn ProtocolSim>,
}

impl GetAmountOutResult {
    /// Constructs a new GetAmountOutResult struct with the given amount and gas
    pub fn new(amount: BigUint, gas: BigUint, new_state: Box<dyn ProtocolSim>) -> Self {
        GetAmountOutResult { amount, gas, new_state }
    }

    /// Aggregates the given GetAmountOutResult struct to the current one.
    /// It updates the amount with the other's amount and adds the other's gas to the current one's
    /// gas.
    pub fn aggregate(&mut self, other: &Self) {
        self.amount = other.amount.clone();
        self.gas += &other.gas;
    }
}

impl fmt::Display for GetAmountOutResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "amount = {}, gas = {}", self.amount, self.gas)
    }
}

/// ProtocolSim trait
/// This trait defines the methods that a protocol state must implement in order to be used
/// in the trade simulation.
pub trait ProtocolSim: fmt::Debug + Send + Sync + 'static {
    /// Returns the fee of the protocol as ratio
    ///
    /// E.g. if the fee is 1%, the value returned would be 0.01.
    fn fee(&self) -> f64;

    /// Returns the protocol's current spot price of two tokens
    ///
    /// Currency pairs are meant to be compared against one another in
    /// order to understand how much of the quote currency is required
    /// to buy one unit of the base currency.
    ///
    /// E.g. if ETH/USD is trading at 1000, we need 1000 USD (quote)
    /// to buy 1 ETH (base currency).
    ///
    /// # Arguments
    ///
    /// * `a` - Base Token: refers to the token that is the quantity of a pair. For the pair
    ///   BTC/USDT, BTC would be the base asset.
    /// * `b` - Quote Token: refers to the token that is the price of a pair. For the symbol
    ///   BTC/USDT, USDT would be the quote asset.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError>;

    /// Returns the amount out given an amount in and input/output tokens.
    ///
    /// # Arguments
    ///
    /// * `amount_in` - The amount in of the input token.
    /// * `token_in` - The input token ERC20 token.
    /// * `token_out` - The output token ERC20 token.
    ///
    /// # Returns
    ///
    /// A `Result` containing a `GetAmountOutResult` struct on success or a
    ///  `SimulationError` on failure.
    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError>;

    /// Computes the maximum amount that can be traded between two tokens.
    ///
    /// This function calculates the maximum possible trade amount between two tokens,
    /// taking into account the protocol's specific constraints and mechanics.
    /// The implementation details vary by protocol - for example:
    /// - For constant product AMMs (like Uniswap V2), this is based on available reserves
    /// - For concentrated liquidity AMMs (like Uniswap V3), this considers liquidity across tick
    ///   ranges
    ///
    /// Note: if there are no limits, the returned amount will be a "soft" limit,
    ///       meaning that the actual amount traded could be higher but it's advised to not
    ///       exceed it.
    ///
    /// # Arguments
    /// * `sell_token` - The address of the token being sold
    /// * `buy_token` - The address of the token being bought
    ///
    /// # Returns
    /// * `Ok((Option<BigUint>, Option<BigUint>))` - A tuple containing:
    ///   - First element: The maximum input amount
    ///   - Second element: The maximum output amount
    ///
    /// This means that for `let res = get_limits(...)` the amount input domain for `get_amount_out`
    /// would be `[0, res.0]` and the amount input domain for `get_amount_in` would be `[0,
    /// res.1]`
    ///
    /// * `Err(SimulationError)` - If any unexpected error occurs
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError>;

    /// Decodes and applies a protocol state delta to the state
    ///
    /// Will error if the provided delta is missing any required attributes or if any of the
    /// attribute values cannot be decoded.
    ///
    /// # Arguments
    ///
    /// * `delta` - A `ProtocolStateDelta` from the tycho indexer
    ///
    /// # Returns
    ///
    /// * `Result<(), TransitionError<String>>` - A `Result` containing `()` on success or a
    ///   `TransitionError` on failure.
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError<String>>;

    /// Clones the protocol state as a trait object.
    /// This allows the state to be cloned when it is being used as a `Box<dyn ProtocolSim>`.
    fn clone_box(&self) -> Box<dyn ProtocolSim>;

    /// Allows downcasting of the trait object to its underlying type.
    fn as_any(&self) -> &dyn Any;

    /// Allows downcasting of the trait object to its mutable underlying type.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Compares two protocol states for equality.
    /// This method must be implemented to define how two protocol states are considered equal
    /// (used for tests).
    fn eq(&self, other: &dyn ProtocolSim) -> bool;

    /// Cast as IndicativelyPriced. This is necessary for RFQ protocols
    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Err(SimulationError::FatalError("Pool State does not implement IndicativelyPriced".into()))
    }
}

impl Clone for Box<dyn ProtocolSim> {
    fn clone(&self) -> Box<dyn ProtocolSim> {
        self.clone_box()
    }
}
