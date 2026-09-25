//! Invoice state machine.
//!
//! States: draft -> open -> {paid | void | uncollectible}, plus draft -> void.
//! See DESIGN.md section 2 for the full diagram and rationale. This module
//! is the single place that decides whether a transition is legal; handlers
//! never mutate `invoices.state` directly without going through here.

use crate::error::AppError;
use axum::http::StatusCode;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceState {
    Draft,
    Open,
    Paid,
    Void,
    Uncollectible,
}

impl InvoiceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            InvoiceState::Draft => "draft",
            InvoiceState::Open => "open",
            InvoiceState::Paid => "paid",
            InvoiceState::Void => "void",
            InvoiceState::Uncollectible => "uncollectible",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(InvoiceState::Draft),
            "open" => Some(InvoiceState::Open),
            "paid" => Some(InvoiceState::Paid),
            "void" => Some(InvoiceState::Void),
            "uncollectible" => Some(InvoiceState::Uncollectible),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            InvoiceState::Paid | InvoiceState::Void | InvoiceState::Uncollectible
        )
    }

    /// draft -> open, triggered by POST /invoices/{id}/finalize.
    pub fn can_finalize(self) -> bool {
        matches!(self, InvoiceState::Draft)
    }

    /// {draft, open} -> void, triggered by POST /invoices/{id}/void.
    pub fn can_void(self) -> bool {
        matches!(self, InvoiceState::Draft | InvoiceState::Open)
    }

    /// open -> uncollectible, triggered by POST /invoices/{id}/mark-uncollectible
    /// (a business decision after collection attempts have failed).
    pub fn can_mark_uncollectible(self) -> bool {
        matches!(self, InvoiceState::Open)
    }

    /// Only an invoice in `open` can be paid. open -> paid is triggered by a
    /// *successful* payment attempt; a failed attempt leaves the invoice in
    /// `open` so it can be retried.
    pub fn can_accept_payment(self) -> bool {
        matches!(self, InvoiceState::Open)
    }
}

impl fmt::Display for InvoiceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

pub fn reject_invalid_transition(action: &str, current: InvoiceState) -> AppError {
    AppError::new(
        StatusCode::CONFLICT,
        "invalid_state_transition",
        format!(
            "cannot {action} invoice while it is in state '{current}'",
        ),
    )
}
