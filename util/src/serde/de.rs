use std::{fmt, rc::Rc};

use lunita::{Table, Value, table::NextValue};
use serde::de;
use thiserror::Error;

use super::markers::{is_none, is_unit};

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("expected {expected}, found {found}")]
    TypeError {
        expected: &'static str,
        found: &'static str,
    },
}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

const DEFAULT_MAX_DEPTH: usize = 128;

/// Options controlling deserialization from Lua values.
#[derive(Debug, Copy, Clone)]
#[non_exhaustive]
pub struct Options {
    /// The maximum number of nested Lua tables that may be traversed.
    ///
    /// The root table, when present, has depth one. A limit of zero rejects all tables but still
    /// permits scalar values.
    pub max_depth: usize,
}

impl Options {
    /// Sets the maximum number of nested Lua tables that may be traversed.
    pub const fn max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

pub fn from_value<'gc, T: de::Deserialize<'gc>>(value: Value<'gc>) -> Result<T, Error> {
    T::deserialize(Deserializer::from_value(value))
}

pub fn from_value_with<'gc, T: de::Deserialize<'gc>>(
    value: Value<'gc>,
    options: Options,
) -> Result<T, Error> {
    T::deserialize(Deserializer::from_value_with(value, options))
}

#[derive(Clone)]
struct DeserializationContext<'gc> {
    active_table: Option<Rc<ActiveTable<'gc>>>,
    max_depth: usize,
}

struct ActiveTable<'gc> {
    table: Table<'gc>,
    parent: Option<Rc<ActiveTable<'gc>>>,
    depth: usize,
}

impl<'gc> DeserializationContext<'gc> {
    fn new(options: Options) -> Self {
        Self {
            active_table: None,
            max_depth: options.max_depth,
        }
    }

    fn enter_table(&self, table: Table<'gc>) -> Result<Self, Error> {
        let mut active_table = self.active_table.clone();
        while let Some(active) = active_table {
            if active.table == table {
                return Err(de::Error::custom(
                    "cyclic table detected during deserialization",
                ));
            }
            active_table = active.parent.clone();
        }

        let current_depth = self.active_table.as_ref().map_or(0, |active| active.depth);
        if current_depth >= self.max_depth {
            return Err(de::Error::custom(format_args!(
                "maximum table nesting depth of {} exceeded",
                self.max_depth
            )));
        }

        Ok(Self {
            active_table: Some(Rc::new(ActiveTable {
                table,
                parent: self.active_table.clone(),
                depth: current_depth + 1,
            })),
            max_depth: self.max_depth,
        })
    }

    fn deserializer(&self, value: Value<'gc>) -> Deserializer<'gc> {
        Deserializer {
            value,
            context: self.clone(),
        }
    }
}

pub struct Deserializer<'gc> {
    value: Value<'gc>,
    context: DeserializationContext<'gc>,
}

impl<'gc> Deserializer<'gc> {
    pub fn from_value(value: Value<'gc>) -> Self {
        Self::from_value_with(value, Options::default())
    }

    pub fn from_value_with(value: Value<'gc>, options: Options) -> Self {
        Self {
            value,
            context: DeserializationContext::new(options),
        }
    }
}

impl<'gc> de::Deserializer<'gc> for Deserializer<'gc> {
    type Error = Error;

    fn deserialize_any<V: de::Visitor<'gc>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.value {
            Value::Nil => self.deserialize_unit(visitor),
            Value::Boolean(_) => self.deserialize_bool(visitor),
            Value::Integer(_) => self.deserialize_i64(visitor),
            Value::Number(_) => self.deserialize_f64(visitor),
            Value::String(s) => {
                if let Ok(string) = s.to_str() {
                    visitor.visit_borrowed_str(string)
                } else {
                    self.deserialize_bytes(visitor)
                }
            }
            Value::Table(t) => {
                if is_sequence(t) {
                    self.deserialize_seq(visitor)
                } else {
                    self.deserialize_map(visitor)
                }
            }
            Value::Function(_) => Err(de::Error::custom("cannot deserialize from function")),
            Value::Thread(_) => Err(de::Error::custom("cannot deserialize from thread")),
            Value::UserData(ud) => {
                if is_none(ud) {
                    self.deserialize_option(visitor)
                } else if is_unit(ud) {
                    self.deserialize_unit(visitor)
                } else {
                    Err(de::Error::custom("cannot deserialize from userdata"))
                }
            }
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        visitor.visit_bool(self.value.to_bool())
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Some(i) = self.value.to_integer() {
            visitor.visit_i64(i)
        } else {
            Err(Error::TypeError {
                expected: "integer",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_f32<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_f64(visitor)
    }

    fn deserialize_f64<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Some(f) = self.value.to_number() {
            visitor.visit_f64(f)
        } else {
            Err(Error::TypeError {
                expected: "number",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::String(s) = self.value {
            if let Ok(s) = s.to_str() {
                visitor.visit_borrowed_str(s)
            } else {
                Err(Error::TypeError {
                    expected: "utf8 string",
                    found: "non-utf8 string",
                })
            }
        } else if self.value.is_implicit_string() {
            visitor.visit_string(self.value.display().to_string())
        } else {
            Err(Error::TypeError {
                expected: "utf8 string",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::String(s) = self.value {
            visitor.visit_borrowed_bytes(s.as_bytes())
        } else {
            Err(Error::TypeError {
                expected: "string",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        match self.value {
            Value::Nil => visitor.visit_none(),
            Value::UserData(ud) if is_none(ud) => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        match self.value {
            Value::Nil => visitor.visit_unit(),
            Value::UserData(ud) if is_unit(ud) => visitor.visit_unit(),
            v => Err(Error::TypeError {
                expected: "nil or unit",
                found: v.type_name(),
            }),
        }
    }

    fn deserialize_unit_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::Table(table) = self.value {
            let context = self.context.enter_table(table)?;
            visitor.visit_seq(Seq::new(table, context))
        } else {
            Err(Error::TypeError {
                expected: "table",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_tuple<V>(self, len: usize, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::Table(table) = self.value {
            let context = self.context.enter_table(table)?;
            visitor.visit_seq(Tuple::new(
                table,
                len.try_into()
                    .map_err(|_| de::Error::custom("tuple length out of range"))?,
                context,
            ))
        } else {
            Err(Error::TypeError {
                expected: "table",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::Table(table) = self.value {
            let context = self.context.enter_table(table)?;
            visitor.visit_map(Map::new(table, context))
        } else {
            Err(Error::TypeError {
                expected: "table",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        match self.value {
            Value::Table(table) => {
                let context = self.context.enter_table(table)?;
                match table.next(Value::Nil) {
                    NextValue::Found { key, value } => {
                        visitor.visit_enum(Enum::new(key, value, context))
                    }
                    NextValue::Last => Err(de::Error::custom("enum table has no entries")),
                    NextValue::NotFound => unreachable!(),
                }
            }
            v => visitor.visit_enum(UnitEnum::new(v, self.context)),
        }
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_any(visitor)
    }
}

pub struct Seq<'gc> {
    table: Table<'gc>,
    ind: i64,
    context: DeserializationContext<'gc>,
}

impl<'gc> Seq<'gc> {
    fn new(table: Table<'gc>, context: DeserializationContext<'gc>) -> Self {
        Self {
            table,
            ind: 1,
            context,
        }
    }
}

impl<'gc> de::SeqAccess<'gc> for Seq<'gc> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Error>
    where
        T: de::DeserializeSeed<'gc>,
    {
        let v = self.table.get_raw(Value::Integer(self.ind));
        if v.is_nil() {
            Ok(None)
        } else {
            let res = Some(seed.deserialize(self.context.deserializer(v))?);
            self.ind = self
                .ind
                .checked_add(1)
                .ok_or(de::Error::custom("index overflow"))?;
            Ok(res)
        }
    }
}

pub struct Tuple<'gc> {
    table: Table<'gc>,
    len: i64,
    ind: i64,
    context: DeserializationContext<'gc>,
}

impl<'gc> Tuple<'gc> {
    fn new(table: Table<'gc>, len: i64, context: DeserializationContext<'gc>) -> Self {
        Self {
            table,
            len,
            ind: 1,
            context,
        }
    }
}

impl<'gc> de::SeqAccess<'gc> for Tuple<'gc> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Error>
    where
        T: de::DeserializeSeed<'gc>,
    {
        if self.ind > self.len {
            Ok(None)
        } else {
            let v = self.table.get_raw(Value::Integer(self.ind));
            let res = Some(seed.deserialize(self.context.deserializer(v))?);
            self.ind += 1;
            Ok(res)
        }
    }
}

pub struct Map<'gc> {
    table: Table<'gc>,
    key: Value<'gc>,
    value: Value<'gc>,
    context: DeserializationContext<'gc>,
}

impl<'gc> Map<'gc> {
    fn new(table: Table<'gc>, context: DeserializationContext<'gc>) -> Self {
        Self {
            table,
            key: Value::Nil,
            value: Value::Nil,
            context,
        }
    }
}

impl<'gc> de::MapAccess<'gc> for Map<'gc> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Error>
    where
        K: de::DeserializeSeed<'gc>,
    {
        match self.table.next(self.key) {
            NextValue::Found { key, value } => {
                self.key = key;
                self.value = value;
                seed.deserialize(self.context.deserializer(self.key))
                    .map(Some)
            }
            NextValue::Last => Ok(None),
            NextValue::NotFound => unreachable!(),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Error>
    where
        V: de::DeserializeSeed<'gc>,
    {
        seed.deserialize(self.context.deserializer(self.value))
    }
}

pub struct Enum<'gc> {
    key: Value<'gc>,
    value: Value<'gc>,
    context: DeserializationContext<'gc>,
}

impl<'gc> Enum<'gc> {
    fn new(key: Value<'gc>, value: Value<'gc>, context: DeserializationContext<'gc>) -> Self {
        Self {
            key,
            value,
            context,
        }
    }
}

impl<'gc> de::EnumAccess<'gc> for Enum<'gc> {
    type Error = Error;
    type Variant = Variant<'gc>;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Variant<'gc>), Error>
    where
        V: de::DeserializeSeed<'gc>,
    {
        Ok((
            seed.deserialize(self.context.deserializer(self.key))?,
            Variant::new(self.value, self.context),
        ))
    }
}

pub struct Variant<'gc> {
    value: Value<'gc>,
    context: DeserializationContext<'gc>,
}

impl<'gc> Variant<'gc> {
    fn new(value: Value<'gc>, context: DeserializationContext<'gc>) -> Self {
        Self { value, context }
    }
}

impl<'gc> de::VariantAccess<'gc> for Variant<'gc> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        de::Deserialize::deserialize(self.context.deserializer(self.value))
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value, Error>
    where
        T: de::DeserializeSeed<'gc>,
    {
        seed.deserialize(self.context.deserializer(self.value))
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        de::Deserializer::deserialize_tuple(self.context.deserializer(self.value), len, visitor)
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        de::Deserializer::deserialize_map(self.context.deserializer(self.value), visitor)
    }
}

pub struct UnitEnum<'gc> {
    key: Value<'gc>,
    context: DeserializationContext<'gc>,
}

impl<'gc> UnitEnum<'gc> {
    fn new(key: Value<'gc>, context: DeserializationContext<'gc>) -> Self {
        Self { key, context }
    }
}

impl<'gc> de::EnumAccess<'gc> for UnitEnum<'gc> {
    type Error = Error;
    type Variant = UnitVariant;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, UnitVariant), Error>
    where
        V: de::DeserializeSeed<'gc>,
    {
        Ok((
            seed.deserialize(self.context.deserializer(self.key))?,
            UnitVariant::new(),
        ))
    }
}

pub struct UnitVariant {}

impl UnitVariant {
    fn new() -> Self {
        Self {}
    }
}

impl<'de> de::VariantAccess<'de> for UnitVariant {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, _seed: T) -> Result<T::Value, Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        Err(Error::TypeError {
            expected: "table",
            found: "non-table",
        })
    }

    fn tuple_variant<V>(self, _len: usize, _visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'de>,
    {
        Err(Error::TypeError {
            expected: "table",
            found: "non-table",
        })
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'de>,
    {
        Err(Error::TypeError {
            expected: "table",
            found: "non-table",
        })
    }
}

fn is_sequence<'gc>(table: Table<'gc>) -> bool {
    let mut key = match table.next(Value::Nil) {
        NextValue::Found { key, value: _ } => key,
        NextValue::Last => return true,
        NextValue::NotFound => unreachable!(),
    };

    let mut ind = 1;
    loop {
        if !matches!(key, Value::Integer(i) if i == ind) {
            return false;
        }

        ind = if let Some(i) = ind.checked_add(1) {
            i
        } else {
            return false;
        };

        key = match table.next(key) {
            NextValue::Found { key, value: _ } => key,
            NextValue::Last => return true,
            NextValue::NotFound => unreachable!(),
        };
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use lunita::{Lua, Table, Value};
    use serde::de::{self, Deserialize, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};

    use super::{Error, Options, from_value, from_value_with};

    #[derive(Debug, PartialEq)]
    enum Nested {
        Integer(i64),
        Sequence(Vec<Nested>),
        Map(Vec<(String, Nested)>),
    }

    impl<'de> Deserialize<'de> for Nested {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: de::Deserializer<'de>,
        {
            deserializer.deserialize_any(NestedVisitor)
        }
    }

    struct NestedVisitor;

    impl<'de> Visitor<'de> for NestedVisitor {
        type Value = Nested;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an integer or nested table")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Nested::Integer(value))
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                values.push(value);
            }
            Ok(Nested::Sequence(values))
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(entry) = map.next_entry()? {
                values.push(entry);
            }
            Ok(Nested::Map(values))
        }
    }

    #[derive(Debug)]
    struct RecursiveEnum;

    impl<'de> Deserialize<'de> for RecursiveEnum {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: de::Deserializer<'de>,
        {
            deserializer.deserialize_enum("RecursiveEnum", &["Node"], RecursiveEnumVisitor)
        }
    }

    struct RecursiveEnumVisitor;

    impl<'de> Visitor<'de> for RecursiveEnumVisitor {
        type Value = RecursiveEnum;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a recursive enum")
        }

        fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
        where
            A: EnumAccess<'de>,
        {
            let (_name, variant) = data.variant::<String>()?;
            variant.newtype_variant::<RecursiveEnum>()?;
            Ok(RecursiveEnum)
        }
    }

    fn assert_cycle(error: Error) {
        assert_eq!(
            error.to_string(),
            "cyclic table detected during deserialization"
        );
    }

    #[test]
    fn recursive_table_adapters_reject_cycles() {
        Lua::core().enter(|ctx| {
            let sequence = Table::new(&ctx);
            sequence
                .set_raw(&ctx, Value::Integer(1), sequence.into())
                .unwrap();
            assert_cycle(from_value::<Nested>(sequence.into()).unwrap_err());
            assert_cycle(from_value::<(Nested,)>(sequence.into()).unwrap_err());

            let first = Table::new(&ctx);
            let second = Table::new(&ctx);
            first
                .set_raw(&ctx, Value::Integer(1), second.into())
                .unwrap();
            second
                .set_raw(&ctx, Value::Integer(1), first.into())
                .unwrap();
            assert_cycle(from_value::<Nested>(first.into()).unwrap_err());

            let map = Table::new(&ctx);
            map.set(ctx, "self", map).unwrap();
            assert_cycle(from_value::<Nested>(map.into()).unwrap_err());

            let enumeration = Table::new(&ctx);
            enumeration.set(ctx, "Node", enumeration).unwrap();
            assert_cycle(from_value::<RecursiveEnum>(enumeration.into()).unwrap_err());
        });
    }

    #[test]
    fn table_depth_limit_is_configurable() {
        Lua::core().enter(|ctx| {
            assert_eq!(
                from_value_with::<i64>(Value::Integer(7), Options::default().max_depth(0)).unwrap(),
                7
            );

            let leaf = Table::new(&ctx);
            let middle = Table::new(&ctx);
            middle
                .set_raw(&ctx, Value::Integer(1), leaf.into())
                .unwrap();
            let root = Table::new(&ctx);
            root.set_raw(&ctx, Value::Integer(1), middle.into())
                .unwrap();

            let error = from_value_with::<Nested>(root.into(), Options::default().max_depth(2))
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "maximum table nesting depth of 2 exceeded"
            );

            let value =
                from_value_with::<Nested>(root.into(), Options::default().max_depth(3)).unwrap();
            assert_eq!(
                value,
                Nested::Sequence(vec![Nested::Sequence(vec![Nested::Sequence(vec![])])])
            );
        });
    }

    #[test]
    fn repeated_acyclic_tables_are_allowed() {
        Lua::core().enter(|ctx| {
            let child = Table::new(&ctx);
            child
                .set_raw(&ctx, Value::Integer(1), Value::Integer(7))
                .unwrap();
            let root = Table::new(&ctx);
            root.set_raw(&ctx, Value::Integer(1), child.into()).unwrap();
            root.set_raw(&ctx, Value::Integer(2), child.into()).unwrap();

            assert_eq!(
                from_value::<Nested>(root.into()).unwrap(),
                Nested::Sequence(vec![
                    Nested::Sequence(vec![Nested::Integer(7)]),
                    Nested::Sequence(vec![Nested::Integer(7)]),
                ])
            );
        });
    }
}
