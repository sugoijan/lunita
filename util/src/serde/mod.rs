pub mod de;
pub mod markers;
pub mod ser;

use lunita::{LimitError, Lua};

pub use self::{
    de::from_value,
    ser::{Options as SerOptions, to_value, to_value_with},
};

pub trait LuaSerdeExt {
    /// Under a memory limit, loading can be refused.
    fn load_serde(&mut self) -> Result<(), LimitError>;
}

impl LuaSerdeExt for Lua {
    fn load_serde(&mut self) -> Result<(), LimitError> {
        self.enter(markers::set_globals)
    }
}
