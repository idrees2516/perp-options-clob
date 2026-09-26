//! Core error types shared across the workspace.

use std::fmt;

/// Errors from exact integer arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathError {
    /// The operation would overflow the result width.
    Overflow,
    /// Division by zero was requested.
    DivideByZero,
}

impl fmt::Display for MathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MathError::Overflow => write!(f, "arithmetic overflow"),
            MathError::DivideByZero => write!(f, "division by zero"),
        }
    }
}

impl std::error::Error for MathError {}

/// Top-level domain errors produced by core types and reused by upper crates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// A math primitive failed.
    Math(MathError),
    /// Referenced instrument does not exist.
    UnknownInstrument,
    /// Order violates instrument constraints (lot size, tick size, bands...).
    InvalidOrder(String),
    /// Account/position referenced by the operation does not exist.
    UnknownAccount,
    /// Non-positive quantity or price where one is required.
    NonPositiveAmount,
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::Math(m) => write!(f, "math error: {m}"),
            CoreError::UnknownInstrument => write!(f, "unknown instrument"),
            CoreError::InvalidOrder(why) => write!(f, "invalid order: {why}"),
            CoreError::UnknownAccount => write!(f, "unknown account"),
            CoreError::NonPositiveAmount => write!(f, "amount must be positive"),
        }
    }
}

impl std::error::Error for CoreError {}

impl From<MathError> for CoreError {
    fn from(m: MathError) -> Self {
        CoreError::Math(m)
    }
}
