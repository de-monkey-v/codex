use serde_json::Map;
use serde_json::Value as JsonValue;

const META_OPENAI_FILE_PARAMS: &str = "openai/fileParams";

pub(crate) fn declared_openai_file_input_param_names(
    meta: Option<&Map<String, JsonValue>>,
) -> Vec<String> {
    declared_top_level_fields(meta, META_OPENAI_FILE_PARAMS)
}

pub(crate) fn mask_input_schema_for_file_path_params(
    input_schema: &mut JsonValue,
    file_params: &[String],
) {
    let Some(properties) = input_schema
        .as_object_mut()
        .and_then(|schema| schema.get_mut("properties"))
        .and_then(JsonValue::as_object_mut)
    else {
        return;
    };

    for field_name in file_params {
        let Some(property_schema) = properties.get_mut(field_name) else {
            continue;
        };
        mask_input_property_schema(property_schema);
    }
}

pub(crate) fn mask_model_visible_tool_input_schema(tool: &mut rmcp::model::Tool) {
    let file_params = declared_openai_file_input_param_names(tool.meta.as_deref());
    if file_params.is_empty() {
        return;
    }

    let mut input_schema = JsonValue::Object(tool.input_schema.as_ref().clone());
    mask_input_schema_for_file_path_params(&mut input_schema, &file_params);
    if let JsonValue::Object(input_schema) = input_schema {
        tool.input_schema = std::sync::Arc::new(input_schema);
    }
}

fn declared_top_level_fields(meta: Option<&Map<String, JsonValue>>, key: &str) -> Vec<String> {
    let Some(meta) = meta else {
        return Vec::new();
    };

    meta.get(key)
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValue::as_str)
        .filter(|value| is_top_level_field_name(value))
        .map(str::to_string)
        .collect()
}

fn is_top_level_field_name(field_name: &str) -> bool {
    !field_name.is_empty()
        && !field_name.contains('.')
        && !field_name.contains('/')
        && !field_name.contains('[')
        && !field_name.contains(']')
}

fn mask_input_property_schema(schema: &mut JsonValue) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    let mut description = object
        .get("description")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let guidance = "This parameter expects an absolute local file path. If you want to upload a file, provide the absolute path to that file here.";
    if description.is_empty() {
        description = guidance.to_string();
    } else if !description.contains(guidance) {
        description = format!("{description} {guidance}");
    }

    let is_array = object.get("type").and_then(JsonValue::as_str) == Some("array")
        || object.get("items").is_some();
    object.clear();
    object.insert("description".to_string(), JsonValue::String(description));
    if is_array {
        object.insert("type".to_string(), JsonValue::String("array".to_string()));
        object.insert("items".to_string(), serde_json::json!({ "type": "string" }));
    } else {
        object.insert("type".to_string(), JsonValue::String("string".to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn declared_openai_file_fields_ignore_nested_names() {
        let meta = serde_json::json!({
            "openai/fileParams": ["file", "nested.value", "files[0]", "attachments"],
            "openai/fileOutputs": ["output", "artifacts/0"]
        });
        let meta = meta.as_object().expect("meta object");

        assert_eq!(
            declared_openai_file_input_param_names(Some(meta)),
            vec!["file".to_string(), "attachments".to_string()]
        );
    }

    #[test]
    fn mask_input_schema_for_file_path_params_rewrites_scalar_and_array_fields() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "file": {
                    "type": "object",
                    "description": "Original file payload."
                },
                "files": {
                    "type": "array",
                    "items": {"type": "object"}
                }
            }
        });

        mask_input_schema_for_file_path_params(
            &mut schema,
            &["file".to_string(), "files".to_string()],
        );

        assert_eq!(
            schema,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Original file payload. This parameter expects an absolute local file path. If you want to upload a file, provide the absolute path to that file here."
                    },
                    "files": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "This parameter expects an absolute local file path. If you want to upload a file, provide the absolute path to that file here."
                    }
                }
            })
        );
    }

    #[test]
    fn mask_model_visible_tool_input_schema_leaves_tool_unchanged_without_declared_params() {
        let original = rmcp::model::Tool {
            name: "echo".into(),
            title: None,
            description: None,
            input_schema: std::sync::Arc::new(
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "object"
                        }
                    }
                })
                .as_object()
                .expect("object")
                .clone(),
            ),
            output_schema: None,
            annotations: None,
            execution: None,
            icons: None,
            meta: None,
        };
        let mut tool = original.clone();

        mask_model_visible_tool_input_schema(&mut tool);

        assert_eq!(tool, original);
    }
}
