use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::error::{Result, SpecLoopError};
use crate::protocol::provider::discriminator;

#[cfg(test)]
mod tests;

/// Encodes a request without letting sorted JSON maps place a union payload before its tag.
/// Only the provider schema is reordered; prompt content and canonical role JSON are untouched.
pub(super) fn encode_request(body: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(&RequestValue {
        value: body,
        schema_path: &["text", "format", "schema"],
    })
    .map_err(|error| SpecLoopError::Agent(format!("serialize OpenAI Responses request: {error}")))
}

struct RequestValue<'a> {
    value: &'a Value,
    schema_path: &'static [&'static str],
}

impl Serialize for RequestValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let Some((next, remaining)) = self.schema_path.split_first() else {
            return SchemaValue {
                value: self.value,
                tag: None,
            }
            .serialize(serializer);
        };
        let Some(object) = self.value.as_object() else {
            return self.value.serialize(serializer);
        };
        let mut output = serializer.serialize_map(Some(object.len()))?;
        for (name, value) in object {
            if name == next {
                output.serialize_entry(
                    name,
                    &Self {
                        value,
                        schema_path: remaining,
                    },
                )?;
            } else {
                output.serialize_entry(name, value)?;
            }
        }
        output.end()
    }
}

struct SchemaValue<'a> {
    value: &'a Value,
    tag: Option<&'a str>,
}

impl Serialize for SchemaValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self.value {
            Value::Object(object) => {
                let mut output = serializer.serialize_map(Some(object.len()))?;
                for (name, value) in object {
                    if matches!(name.as_str(), "properties" | "$defs") {
                        let schemas = value.as_object().ok_or_else(|| {
                            serde::ser::Error::custom(
                                "schema properties or definitions must be an object",
                            )
                        })?;
                        let first = if name == "properties" { self.tag } else { None };
                        output.serialize_entry(name, &SchemaMap { schemas, first })?;
                    } else {
                        output.serialize_entry(name, &Self { value, tag: None })?;
                    }
                }
                output.end()
            }
            Value::Array(values) => {
                // project_schema has already validated these union alternatives. Reuse its
                // discriminator selection rather than duplicating status/outcome role policy.
                let tag = discriminator(values);
                let mut output = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    output.serialize_element(&Self { value, tag })?;
                }
                output.end()
            }
            value => value.serialize(serializer),
        }
    }
}

struct SchemaMap<'a> {
    schemas: &'a Map<String, Value>,
    first: Option<&'a str>,
}

impl Serialize for SchemaMap<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut output = serializer.serialize_map(Some(self.schemas.len()))?;
        if let Some(first) = self.first {
            let value = self.schemas.get(first).ok_or_else(|| {
                serde::ser::Error::custom("schema union alternative omitted its discriminator")
            })?;
            output.serialize_entry(first, &SchemaValue { value, tag: None })?;
        }
        for (name, value) in self.schemas {
            if self.first != Some(name.as_str()) {
                output.serialize_entry(name, &SchemaValue { value, tag: None })?;
            }
        }
        output.end()
    }
}
