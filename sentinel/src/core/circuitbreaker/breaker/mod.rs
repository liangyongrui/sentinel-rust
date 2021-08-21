//!  Circuit Breaker State Machine:
//!
//!                                switch to open based on rule
//!				+-----------------------------------------------------------------------+
//!				|                                                                       |
//!				|                                                                       v
//!		+----------------+                   +----------------+      Probe      +----------------+
//!		|                |                   |                |<----------------|                |
//!		|                |   Probe succeed   |                |                 |                |
//!		|     Closed     |<------------------|    HalfOpen    |                 |      Open      |
//!		|                |                   |                |   Probe failed  |                |
//!		|                |                   |                +---------------->|                |
//!		+----------------+                   +----------------+                 +----------------+

/// Error count
pub mod error_count;
/// Error ratio
pub mod error_ratio;
/// Slow round trip time
pub mod slow_request;
pub mod stat;

pub use error_count::*;
pub use error_ratio::*;
pub use slow_request::*;
pub use stat::*;

use super::*;
use crate::{
    base::{EntryContext, SentinelEntry, Snapshot},
    logging,
    stat::{LeapArray, MetricTrait},
    utils, Error, Result,
};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::hash::Hash;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

/// `BreakerStrategy` represents the strategy of circuit breaker.
/// Each strategy is associated with one rule type.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum BreakerStrategy {
    /// `SlowRequestRatio` strategy changes the circuit breaker state based on slow request ratio
    SlowRequestRatio,
    /// `ErrorRatio` strategy changes the circuit breaker state based on error request ratio
    ErrorRatio,
    /// `ErrorCount` strategy changes the circuit breaker state based on error amount
    ErrorCount,
    Custom(u8),
}

impl Default for BreakerStrategy {
    fn default() -> BreakerStrategy {
        BreakerStrategy::SlowRequestRatio
    }
}

/// States of Circuit Breaker State Machine
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum State {
    Closed,
    HalfOpen,
    Open,
}

impl Default for State {
    fn default() -> State {
        State::Closed
    }
}

impl State {}

/// `StateChangeListener` listens on the circuit breaker state change event
pub trait StateChangeListener: Sync + Send {
    /// on_transform_to_closed is triggered when circuit breaker state transformed to Closed.
    /// Argument rule is copy from circuit breaker's rule, any changes of rule don't take effect for circuit breaker
    /// Copying rule has a performance penalty and avoids invalid listeners as much as possible
    fn on_transform_to_closed(&self, prev: State, rule: Arc<Rule>);

    /// `on_transform_to_open` is triggered when circuit breaker state transformed to Open.
    /// The "snapshot" indicates the triggered value when the transformation occurs.
    /// Argument rule is copy from circuit breaker's rule, any changes of rule don't take effect for circuit breaker
    /// Copying rule has a performance penalty and avoids invalid listeners as much as possible
    fn on_transform_to_open(&self, prev: State, rule: Arc<Rule>, snapshot: Option<Arc<Snapshot>>);

    /// `on_transform_to_half_open` is triggered when circuit breaker state transformed to HalfOpen.
    /// Argument rule is copy from circuit breaker's rule, any changes of rule don't take effect for circuit breaker
    /// Copying rule has a performance penalty and avoids invalid listeners as much as possible
    fn on_transform_to_half_open(&self, prev: State, rule: Arc<Rule>);
}

/// `CircuitBreakerTrait` is the basic trait of circuit breaker
pub trait CircuitBreakerTrait: Send + Sync {
    /// `bound_rule` returns the associated circuit breaking rule.
    fn bound_rule(&self) -> &Arc<Rule>;
    /// `stat` returns the associated statistic data structure.
    fn stat(&self) -> &Arc<CounterLeapArray>;
    /// `try_pass` acquires permission of an invocation only if it is available at the time of invocation.
    /// it checks circuit breaker based on state machine of circuit breaker.
    fn try_pass(&self, ctx: Rc<RefCell<EntryContext>>) -> bool;
    /// `current_state` returns current state of the circuit breaker.
    fn current_state(&self) -> State;
    /// `on_request_complete` record a completed request with the given response time as well as error (if present),
    /// and handle state transformation of the circuit breaker.
    /// `on_request_complete` is called only when a passed invocation finished.
    fn on_request_complete(&self, rt: u64, error: &Option<Error>);
    // the underlying metric should be with inner-mutability, thus, here we use `&self`
    fn reset_metric(&self);
}

/// BreakerBase encompasses the common fields of circuit breaker.
#[derive(Debug)]
pub struct BreakerBase {
    rule: Arc<Rule>,
    /// retry_timeout_ms represents recovery timeout (in milliseconds) before the circuit breaker opens.
    /// During the open period, no requests are permitted until the timeout has elapsed.
    /// After that, the circuit breaker will transform to half-open state for trying a few "trial" requests.
    retry_timeout_ms: u32,
    /// next_retry_timestamp_ms is the time circuit breaker could probe
    next_retry_timestamp_ms: AtomicU64,
    /// state is the state machine of circuit breaker
    state: Arc<Mutex<State>>,
}

impl BreakerBase {
    pub fn bound_rule(&self) -> &Arc<Rule> {
        &self.rule
    }

    pub fn current_state(&self) -> State {
        *self.state.lock().unwrap()
    }

    pub fn retry_timeout_arrived(&self) -> bool {
        utils::curr_time_millis() >= self.next_retry_timestamp_ms.load(Ordering::SeqCst)
    }

    pub fn update_next_retry_timestamp(&self) {
        self.next_retry_timestamp_ms.store(
            utils::curr_time_millis() + self.retry_timeout_ms as u64,
            Ordering::SeqCst,
        );
    }

    /// from_closed_to_open updates circuit breaker state machine from closed to open.
    /// Return true only if current goroutine successfully accomplished the transformation.
    pub fn from_closed_to_open(&self, snapshot: Arc<Snapshot>) -> bool {
        let mut state = self.state.lock().unwrap();
        if *state == State::Closed {
            *state = State::Open;
            self.update_next_retry_timestamp();
            let listeners = state_change_listeners().lock().unwrap();
            for listener in &*listeners {
                listener.on_transform_to_open(
                    State::Closed,
                    Arc::clone(&self.rule),
                    Some(Arc::clone(&snapshot)),
                );
            }
            true
        } else {
            false
        }
    }

    /// from_open_to_half_open updates circuit breaker state machine from open to half-open.
    /// Return true only if current goroutine successfully accomplished the transformation.
    pub fn from_open_to_half_open(&self, ctx: Rc<RefCell<EntryContext>>) -> bool {
        let mut state = self.state.lock().unwrap();
        if *state == State::Open {
            *state = State::HalfOpen;
            let listeners = state_change_listeners().lock().unwrap();
            for listener in &*listeners {
                listener.on_transform_to_half_open(State::Open, Arc::clone(&self.rule));
            }

            let entry = ctx.borrow().entry();
            if entry.is_none() {
                logging::error!(
                    "Entry is None in BreakerBase::from_open_to_half_open(), rule: {:?}",
                    self.rule,
                );
            } else {
                // add hook for entry exit
                // if the current circuit breaker performs the probe through this entry, but the entry was blocked,
                // this hook will guarantee current circuit breaker state machine will rollback to Open from Half-Open
                drop(state);
                let entry = entry.unwrap();
                let rule = Arc::clone(&self.rule);
                let state = Arc::clone(&self.state);
                Rc::get_mut(&mut entry.upgrade().unwrap())
                    .unwrap()
                    .when_exit(Box::new(
                        move |entry: &SentinelEntry,
                              ctx: Rc<RefCell<EntryContext>>|
                              -> Result<()> {
                            let mut state = state.lock().unwrap();
                            if ctx.borrow().is_blocked() && *state == State::HalfOpen {
                                *state = State::Open;
                                let listeners = state_change_listeners().lock().unwrap();
                                for listener in &*listeners {
                                    listener.on_transform_to_open(
                                        State::HalfOpen,
                                        Arc::clone(&rule),
                                        Some(Arc::new(1.0)),
                                    );
                                }
                            }
                            Ok(())
                        },
                    ))
            }
            true
        } else {
            false
        }
    }

    /// from_half_open_to_open updates circuit breaker state machine from half-open to open.
    /// Return true only if current goroutine successfully accomplished the transformation.
    pub fn from_half_open_to_open(&self, snapshot: Arc<Snapshot>) -> bool {
        let mut state = self.state.lock().unwrap();
        if *state == State::HalfOpen {
            *state = State::Open;
            self.update_next_retry_timestamp();
            let listeners = state_change_listeners().lock().unwrap();
            for listener in &*listeners {
                listener.on_transform_to_open(
                    State::HalfOpen,
                    Arc::clone(&self.rule),
                    Some(Arc::clone(&snapshot)),
                );
            }
            true
        } else {
            false
        }
    }

    /// from_half_open_to_closed updates circuit breaker state machine from half-open to closed
    /// Return true only if current goroutine successfully accomplished the transformation.
    pub fn from_half_open_to_closed(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if *state == State::HalfOpen {
            *state = State::Closed;
            let listeners = state_change_listeners().lock().unwrap();
            for listener in &*listeners {
                listener.on_transform_to_closed(State::HalfOpen, Arc::clone(&self.rule));
            }
            true
        } else {
            false
        }
    }
}
