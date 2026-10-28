use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::error::{Result, SpecLoopError};
#[cfg(test)]
use crate::protocol;

#[cfg(test)]
mod tests;

const SUPPORTED_KEYWORDS: &[&str] = &[
    "$defs",
    "$ref",
    "additionalProperties",
    "anyOf",
    "description",
    "enum",
    "format",
    "items",
    "maximum",
    "maxItems",
    "minimum",
    "minItems",
    "pattern",
    "properties",
    "required",
    "type",
];

#[cfg(test)]
pub(super) fn schema<T: schemars::JsonSchema>() -> Result<Value> {
    project_schema(&protocol::schema_value::<T>()?)
}

/// Mechanically projects a canonical role schema into the strict provider subset.
pub fn project_schema(canonical: &Value) -> Result<Value> {
    let canonical_object = canonical.as_object().ok_or_else(|| {
        SpecLoopError::Agent("canonical role schema root is not an object".to_owned())
    })?;
    for keyword in canonical_object.keys() {
        if !matches!(
            keyword.as_str(),
            "$defs" | "$schema" | "description" | "oneOf" | "title"
        ) {
            return Err(schema_error(
                "$",
                &format!("unsupported root keyword {keyword:?}"),
            ));
        }
    }
    let branches = canonical
        .get("oneOf")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SpecLoopError::Agent(
                "canonical role schema must have status-discriminated oneOf branches".to_owned(),
            )
        })?;
    ensure_disjoint_statuses(branches)?;
    let projected = branches
        .iter()
        .enumerate()
        .map(|(index, branch)| transform_schema(branch, &format!("oneOf[{index}]")))
        .collect::<Result<Vec<_>>>()?;
    let mut root = Map::new();
    root.insert("type".to_owned(), Value::String("object".to_owned()));
    root.insert(
        "properties".to_owned(),
        json!({"response": {"anyOf": projected}}),
    );
    root.insert("required".to_owned(), json!(["response"]));
    root.insert("additionalProperties".to_owned(), Value::Bool(false));
    if let Some(description) = canonical.get("description") {
        if !description.is_string() {
            return Err(schema_error("$", "schema description must be a string"));
        }
        root.insert("description".to_owned(), description.clone());
    }
    if let Some(definitions) = canonical.get("$defs") {
        root.insert(
            "$defs".to_owned(),
            transform_schema_map(definitions, "$defs")?,
        );
    }
    let projected = Value::Object(root);
    validate_references(&projected, &projected, "$")?;
    Ok(projected)
}

fn transform_schema(value: &Value, path: &str) -> Result<Value> {
    let Some(object) = value.as_object() else {
        if let Some(array) = value.as_array() {
            return array
                .iter()
                .enumerate()
                .map(|(index, item)| transform_schema(item, &format!("{path}[{index}]")))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array);
        }
        return Ok(value.clone());
    };

    if object.contains_key("$ref") {
        return transform_reference(object, path);
    }
    let mut transformed = Map::new();
    for (keyword, child) in object {
        if let Some((projected_keyword, projected_value)) =
            transform_keyword(keyword, child, object, path)?
        {
            transformed.insert(projected_keyword, projected_value);
        }
    }
    ensure_strict_object(&mut transformed, path)?;
    Ok(Value::Object(transformed))
}

fn transform_reference(schema: &Map<String, Value>, path: &str) -> Result<Value> {
    let reference = schema
        .get("$ref")
        .and_then(Value::as_str)
        .ok_or_else(|| schema_error(path, "schema reference must be a string"))?;
    // The provider forbids every $ref sibling. Omit documentation annotations only;
    // constraints must fail closed instead of disappearing from the projected contract.
    for (keyword, value) in schema {
        match keyword.as_str() {
            "$ref" => {}
            "description" | "title" | "$schema" if value.is_string() => {}
            "description" | "title" | "$schema" => {
                return Err(schema_error(path, "reference annotation must be a string"));
            }
            _ => {
                return Err(schema_error(
                    path,
                    &format!("unsupported keyword {keyword:?} beside $ref"),
                ));
            }
        }
    }
    Ok(json!({"$ref":reference}))
}

fn transform_keyword(
    keyword: &str,
    child: &Value,
    schema: &Map<String, Value>,
    path: &str,
) -> Result<Option<(String, Value)>> {
    let transformed = match keyword {
        "$schema" | "title" => return Ok(None),
        "minLength" | "maxLength" => {
            // The provider subset omits string-length bounds; canonical local parsing retains them.
            if child.as_u64().is_none() {
                return Err(schema_error(
                    path,
                    &format!("{keyword} must be a non-negative integer"),
                ));
            }
            return Ok(None);
        }
        "const" => {
            if schema.contains_key("enum") {
                return Err(schema_error(
                    path,
                    "const and enum cannot be combined safely",
                ));
            }
            ("enum", Value::Array(vec![child.clone()]))
        }
        "format" => {
            let format = child
                .as_str()
                .ok_or_else(|| schema_error(path, "format must be a string"))?;
            let value_type = schema.get("type").and_then(Value::as_str);
            let retain = match (value_type, format) {
                // Schemars emits these Rust integer formats; preserve numeric bounds in the schema
                // and rely on canonical local deserialization for the unsigned Rust range.
                (Some("integer"), "uint32" | "uint64") => false,
                (
                    Some("string"),
                    "date-time" | "time" | "date" | "duration" | "email" | "hostname" | "ipv4"
                    | "ipv6" | "uuid",
                ) => true,
                _ => {
                    return Err(schema_error(
                        path,
                        &format!("unsupported format {format:?}"),
                    ));
                }
            };
            if !retain {
                return Ok(None);
            }
            (keyword, child.clone())
        }
        "properties" | "$defs" => (
            keyword,
            transform_schema_map(child, &format!("{path}.{keyword}"))?,
        ),
        "oneOf" => {
            if schema.contains_key("anyOf") {
                return Err(schema_error(
                    path,
                    "oneOf and anyOf cannot be combined safely",
                ));
            }
            let branches = child
                .as_array()
                .ok_or_else(|| schema_error(path, "oneOf must be an array"))?;
            ensure_disjoint_statuses(branches)?;
            let projected = branches
                .iter()
                .enumerate()
                .map(|(index, branch)| transform_schema(branch, &format!("{path}.oneOf[{index}]")))
                .collect::<Result<Vec<_>>>()?;
            ("anyOf", Value::Array(projected))
        }
        _ if SUPPORTED_KEYWORDS.contains(&keyword) => (
            keyword,
            transform_schema(child, &format!("{path}.{keyword}"))?,
        ),
        _ => {
            return Err(schema_error(
                path,
                &format!("unsupported keyword {keyword:?}"),
            ));
        }
    };
    Ok(Some((transformed.0.to_owned(), transformed.1)))
}

fn ensure_strict_object(schema: &mut Map<String, Value>, path: &str) -> Result<()> {
    if schema.get("type").and_then(Value::as_str) != Some("object")
        && !schema.contains_key("properties")
    {
        return Ok(());
    }
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| schema_error(path, "object schema has no properties object"))?;
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .ok_or_else(|| schema_error(path, "object schema has no required array"))?;
    let property_names = properties
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let required_names = required
        .iter()
        .map(|field| {
            field
                .as_str()
                .ok_or_else(|| schema_error(path, "required field is not a string"))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if property_names != required_names {
        return Err(schema_error(
            path,
            "strict provider schema requires every property to be canonically required",
        ));
    }
    if schema
        .get("additionalProperties")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err(schema_error(
            path,
            "object schema permits additional properties",
        ));
    }
    schema.insert("additionalProperties".to_owned(), Value::Bool(false));
    Ok(())
}

fn transform_schema_map(value: &Value, path: &str) -> Result<Value> {
    let object = value
        .as_object()
        .ok_or_else(|| schema_error(path, "expected object mapping names to schemas"))?;
    let mut transformed = Map::new();
    for (name, schema) in object {
        transformed.insert(
            name.clone(),
            transform_schema(schema, &format!("{path}.{name}"))?,
        );
    }
    Ok(Value::Object(transformed))
}

/// Finds a tagged union's finite discriminator for validation and provider wire ordering.
/// Scalar enum alternatives have no object discriminator. Callers validate disjointness separately.
pub fn discriminator(branches: &[Value]) -> Option<&str> {
    branches
        .first()
        .and_then(|branch| branch.get("properties"))
        .and_then(Value::as_object)
        .and_then(|properties| {
            properties.iter().find(|(_, property)| {
                property.get("const").is_some() || property.get("enum").is_some()
            })
        })
        .map(|(name, _)| name.as_str())
}

fn ensure_disjoint_statuses(branches: &[Value]) -> Result<()> {
    // Nested adjudication uses outcome; derive the tag from the schema, not role policy.
    let tag = discriminator(branches);
    let mut seen = BTreeSet::new();
    for (index, branch) in branches.iter().enumerate() {
        let discriminator = if let Some(tag) = tag {
            branch
                .get("properties")
                .and_then(|properties| properties.get(tag))
                .ok_or_else(|| {
                    schema_error(
                        "oneOf",
                        "canonical union branches have different discriminators",
                    )
                })?
        } else {
            branch
        };
        let values = if let Some(value) = discriminator.get("const") {
            vec![value]
        } else {
            discriminator
                .get("enum")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    SpecLoopError::Agent(format!(
                        "canonical union branch {index} has no finite status discriminator"
                    ))
                })?
                .iter()
                .collect()
        };
        for value in values {
            if !seen.insert(value.to_string()) {
                return Err(SpecLoopError::Agent(
                    "canonical union alternatives are not pairwise disjoint".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_references(value: &Value, root: &Value, path: &str) -> Result<()> {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                let pointer = reference
                    .strip_prefix('#')
                    .ok_or_else(|| schema_error(path, "provider schema reference is not local"))?;
                if !reference.starts_with("#/") || root.pointer(pointer).is_none() {
                    return Err(schema_error(
                        path,
                        "provider schema reference does not resolve",
                    ));
                }
            }
            for (key, child) in object {
                validate_references(child, root, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                validate_references(child, root, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Removes only the exact strict-output transport envelope, preserving canonical role fields.
/// Normalizes only the documented provider wrapper containing one canonical role object.
pub fn normalize_envelope(value: &Value) -> Result<Value> {
    let object = value.as_object().ok_or_else(|| {
        SpecLoopError::Agent("structured role response envelope must be an object".to_owned())
    })?;
    if object.len() != 1 || !object.contains_key("response") {
        return Err(SpecLoopError::Agent(
            "structured role response envelope must contain only the `response` field".to_owned(),
        ));
    }
    let canonical = object.get("response").ok_or_else(|| {
        SpecLoopError::Agent("structured role response envelope omitted `response`".to_owned())
    })?;
    if !canonical.is_object() {
        return Err(SpecLoopError::Agent(
            "structured role response envelope `response` must be an object".to_owned(),
        ));
    }
    Ok(canonical.clone())
}

fn schema_error(path: &str, detail: &str) -> SpecLoopError {
    SpecLoopError::Agent(format!(
        "cannot project canonical role schema at {path}: {detail}"
    ))
}
