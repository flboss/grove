
pub mod aggs;
pub mod assemble;
pub mod conn;
pub mod decode;
pub mod error;
pub mod exec;
pub mod funcs;
pub mod params;
pub mod temp;
pub mod tvf;
pub mod value;

pub use assemble::assemble;
pub use conn::Runtime;
pub use decode::{assemble_dec, decode_dec, decode_duration, decode_instant};
pub use error::RuntimeError;
pub use exec::{Row, StatementOutput};
pub use value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IntArithmetic {
    #[default]
    Checked,
    Saturating,
    Wrapping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecArithmetic {
    #[default]
    Checked,
    Saturating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub int_arithmetic: IntArithmetic,
    pub float_checks: bool,
    pub dec_arithmetic: DecArithmetic,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            int_arithmetic: IntArithmetic::Checked,
            float_checks: true,
            dec_arithmetic: DecArithmetic::Checked,
        }
    }
}
