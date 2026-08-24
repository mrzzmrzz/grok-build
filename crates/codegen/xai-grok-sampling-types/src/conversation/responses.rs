//! Responses API wire format.

use super::*;

use crate::types::ProviderProfile;

/// Flatten `response.output` into `ConversationItem`s, preserving emission
/// order. Replaying that order byte for byte on the next turn is what keeps
/// the server-side prefix cache hot.
pub fn response_to_conversation_items(response: rs::Response) -> Vec<ConversationItem> {
    response_to_conversation_items_with_client_custom_tools(response, &[])
}

/// Convert a Responses response while distinguishing client-executed native
/// custom tools from xAI's backend custom-tool carrier. The typed Responses
/// dependency uses the same output item for both, so the declared client tool
/// names are the narrow discriminator available at this boundary.
pub fn response_to_conversation_items_with_client_custom_tools(
    response: rs::Response,
    client_custom_tool_names: &[String],
) -> Vec<ConversationItem> {
    let model_id = response.model.clone();
    let model_fingerprint = response
        .metadata
        .as_ref()
        .and_then(|m| m.get("system_fingerprint"))
        .cloned()
        .filter(|s| !s.is_empty());
    let reasoning_effort = response
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.clone())
        .map(crate::ReasoningEffort::from_responses_api);

    let mut items: Vec<ConversationItem> = Vec::with_capacity(response.output.len() + 1);
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut backend_tool_count: usize = 0;

    for item in response.output {
        match item {
            rs::OutputItem::Message(msg) => {
                for content_part in msg.content {
                    if let rs::OutputMessageContent::OutputText(text_content) = content_part {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&text_content.text);
                    }
                }
            }
            rs::OutputItem::FunctionCall(fc) => {
                // Tied to the assistant turn: a ToolResult must follow each
                // one in conversation order, so they are not siblings.
                tool_calls.push(ToolCall {
                    id: Arc::<str>::from(fc.call_id),
                    name: fc.name,
                    arguments: Arc::<str>::from(fc.arguments),
                });
            }
            rs::OutputItem::Reasoning(r) => {
                items.push(ConversationItem::Reasoning(r));
            }
            // Already run server-side; kept so later turns replay the same
            // context.
            rs::OutputItem::WebSearchCall(ws) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::WebSearch(ws),
                }));
            }
            rs::OutputItem::CustomToolCall(ct) => {
                if client_custom_tool_names.iter().any(|name| name == &ct.name) {
                    tool_calls.push(ToolCall::custom(
                        ct.call_id,
                        ct.id,
                        ct.name,
                        ct.input,
                    ));
                } else {
                    backend_tool_count += 1;
                    items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                        kind: BackendToolKind::XSearch(ct),
                    }));
                }
            }
            rs::OutputItem::CodeInterpreterCall(ci) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::CodeInterpreter(ci),
                }));
            }
            rs::OutputItem::McpCall(_) => {
                backend_tool_count += 1;
            }
            _ => {}
        }
    }

    if backend_tool_count > 0 {
        tracing::info!(
            backend_tool_count,
            "response contained backend-executed tool calls"
        );
    }

    tracing::info!(model_id = %model_id, ?model_fingerprint, ?reasoning_effort, "response_to_conversation_items setting model metadata on AssistantItem");
    items.push(ConversationItem::Assistant(AssistantItem {
        content: Arc::<str>::from(content),
        tool_calls,
        model_id: Some(model_id),
        model_fingerprint,
        reasoning_effort,
    }));

    items
}

impl From<&ConversationRequest> for rs::CreateResponse {
    fn from(req: &ConversationRequest) -> Self {
        let input = build_responses_input(req);
        let tools = build_responses_tools(req);

        let tool_choice = req.tool_choice.as_ref().map(|tc| match tc {
            ConversationToolChoice::Auto => rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Auto),
            ConversationToolChoice::None => rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::None),
            ConversationToolChoice::Required => {
                rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Required)
            }
            ConversationToolChoice::Function(name) => {
                rs::ToolChoiceParam::Function(rs::ToolChoiceFunction { name: name.clone() })
            }
            ConversationToolChoice::Custom(name) => {
                rs::ToolChoiceParam::Custom(rs::ToolChoiceCustom { name: name.clone() })
            }
        });

        let text = req
            .json_schema
            .as_ref()
            .map(|schema| rs::ResponseTextParam {
                format: rs::TextResponseFormatConfiguration::JsonSchema(
                    rs::ResponseFormatJsonSchema {
                        description: None,
                        name: STRUCTURED_OUTPUT_SCHEMA_NAME.to_string(),
                        schema: Some(schema.clone()),
                        strict: Some(true),
                    },
                ),
                verbosity: None,
            });

        rs::CreateResponse {
            background: None,
            conversation: None,
            include: None,
            input,
            instructions: None,
            max_output_tokens: req.max_output_tokens,
            max_tool_calls: None,
            metadata: None,
            model: req.model.clone(),
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: req
                .prompt_cache_key
                .clone()
                .or_else(|| req.x_grok_conv_id.clone()),
            prompt_cache_retention: None,
            reasoning: Some(rs::Reasoning {
                effort: req.reasoning_effort.map(|e| e.to_responses_api()),
                summary: Some(rs::ReasoningSummary::Concise),
            }),
            safety_identifier: None,
            service_tier: None,
            store: None,
            stream: None,
            stream_options: None,
            temperature: req.temperature,
            text,
            tool_choice,
            tools: if tools.is_empty() { None } else { Some(tools) },
            top_logprobs: None,
            top_p: req.top_p,
            truncation: None,
        }
    }
}

/// Reasoning items stay top-level siblings rather than folding into the
/// assistant, so the input replays the model's original order.
pub(super) fn build_responses_input(req: &ConversationRequest) -> rs::InputParam {
    let items: Vec<rs::InputItem> = req
        .items
        .iter()
        .flat_map(conversation_item_to_input_items)
        .collect();
    rs::InputParam::Items(items)
}

/// Inject the `type: "reasoning_text"` discriminator the API requires.
/// `async-openai`'s `ReasoningTextContent` has no `type` field, so it
/// serializes to `{"text": ...}` and the API answers 400. Delete this once
/// upstream grows the field.
pub fn patch_reasoning_text_types(body: &mut serde_json::Value) {
    let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for item in input.iter_mut() {
        if item.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
            continue;
        }
        let Some(content) = item.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for c in content.iter_mut() {
            if let Some(obj) = c.as_object_mut() {
                obj.entry("type")
                    .or_insert_with(|| serde_json::Value::String("reasoning_text".into()));
            }
        }
    }
}

fn conversation_item_to_input_items(item: &ConversationItem) -> Vec<rs::InputItem> {
    match item {
        ConversationItem::System(s) => {
            vec![rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::System,
                content: rs::EasyInputContent::Text(s.content.as_ref().to_owned()),
            })]
        }
        ConversationItem::User(u) => {
            let content = content_parts_to_easy_input_content(&u.content);
            vec![rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::User,
                content,
            })]
        }
        ConversationItem::Reasoning(r) => {
            // `status` is output-only and rejected on input.
            let mut r = r.clone();
            r.status = None;
            vec![rs::InputItem::Item(rs::Item::Reasoning(r))]
        }
        ConversationItem::Assistant(a) => {
            let mut items = Vec::new();

            if !a.content.is_empty() {
                items.push(rs::InputItem::EasyMessage(rs::EasyInputMessage {
                    r#type: rs::MessageType::Message,
                    role: rs::Role::Assistant,
                    content: rs::EasyInputContent::Text(a.content.as_ref().to_owned()),
                }));
            }

            for tc in &a.tool_calls {
                if tc.is_custom() {
                    let custom_call: rs::CustomToolCall = serde_json::from_value(
                        serde_json::json!({
                            "call_id": tc.call_id(),
                            "input": tc.arguments.as_ref(),
                            "name": tc.name,
                            "id": tc.custom_item_id().unwrap_or(tc.call_id()),
                        }),
                    )
                    .expect("native custom tool call fields must satisfy the Responses schema");
                    items.push(rs::InputItem::Item(rs::Item::CustomToolCall(custom_call)));
                } else {
                    let arguments =
                        sanitize_tool_arguments(&tc.id, &tc.name, tc.arguments.clone());
                    items.push(rs::InputItem::Item(rs::Item::FunctionCall(
                        rs::FunctionToolCall {
                            call_id: tc.id.as_ref().to_owned(),
                            name: tc.name.clone(),
                            arguments: arguments.as_ref().to_owned(),
                            id: None,
                            status: None,
                        },
                    )));
                }
            }

            items
        }
        ConversationItem::ToolResult(t) => {
            // A non-empty `parts` carries the authoritative interleaved
            // order; a single text part keeps the plain-text wire shape.
            // Empty `parts` keeps the legacy text-then-images layout.
            let list_content = || tool_result_input_content(t);
            if let Some((call_id, _)) = decode_custom_tool_call_id(&t.tool_call_id) {
                let output = if let [ContentPart::Text { text }] = t.parts.as_slice() {
                    rs::CustomToolCallOutputOutput::Text(text.as_ref().to_owned())
                } else if !t.parts.is_empty() {
                    rs::CustomToolCallOutputOutput::List(list_content())
                } else if t.images.is_empty() {
                    rs::CustomToolCallOutputOutput::Text(t.content.as_ref().to_owned())
                } else {
                    rs::CustomToolCallOutputOutput::List(list_content())
                };
                return vec![rs::InputItem::Item(rs::Item::CustomToolCallOutput(
                    rs::CustomToolCallOutput {
                        call_id: call_id.to_owned(),
                        output,
                        id: None,
                    },
                ))];
            }
            let output = if let [ContentPart::Text { text }] = t.parts.as_slice() {
                rs::FunctionCallOutput::Text(text.as_ref().to_owned())
            } else if !t.parts.is_empty() {
                rs::FunctionCallOutput::Content(list_content())
            } else if t.images.is_empty() {
                rs::FunctionCallOutput::Text(t.content.as_ref().to_owned())
            } else {
                rs::FunctionCallOutput::Content(list_content())
            };
            vec![rs::InputItem::Item(rs::Item::FunctionCallOutput(
                rs::FunctionCallOutputItemParam {
                    call_id: t.tool_call_id.clone(),
                    output,
                    id: None,
                    status: None,
                },
            ))]
        }
        ConversationItem::BackendToolCall(b) => {
            vec![match &b.kind {
                BackendToolKind::WebSearch(ws) => {
                    rs::InputItem::Item(rs::Item::WebSearchCall(ws.clone()))
                }
                BackendToolKind::XSearch(ct) => {
                    rs::InputItem::Item(rs::Item::CustomToolCall(ct.clone()))
                }
                BackendToolKind::CodeInterpreter(ci) => {
                    rs::InputItem::Item(rs::Item::CodeInterpreterCall(ci.clone()))
                }
            }]
        }
    }
}

/// Ordered `InputContent` for a tool result: `parts` verbatim when present,
/// otherwise the legacy text-then-images layout.
fn tool_result_input_content(t: &ToolResultItem) -> Vec<rs::InputContent> {
    if !t.parts.is_empty() {
        return t.parts.iter().map(content_part_to_input_content).collect();
    }
    let mut out = vec![rs::InputContent::InputText(rs::InputTextContent {
        text: t.content.as_ref().to_owned(),
    })];
    out.extend(
        t.images
            .iter()
            .filter(|p| matches!(p, ContentPart::Image { .. }))
            .map(content_part_to_input_content),
    );
    out
}

fn content_part_to_input_content(part: &ContentPart) -> rs::InputContent {
    match part {
        ContentPart::Text { text } => rs::InputContent::InputText(rs::InputTextContent {
            text: text.as_ref().to_owned(),
        }),
        ContentPart::Image { url } => rs::InputContent::InputImage(rs::InputImageContent {
            detail: rs::ImageDetail::Auto,
            file_id: None,
            image_url: Some(url.as_ref().to_owned()),
        }),
    }
}

fn content_parts_to_easy_input_content(parts: &[ContentPart]) -> rs::EasyInputContent {
    if parts.len() == 1
        && let ContentPart::Text { text } = &parts[0]
    {
        return rs::EasyInputContent::Text(text.as_ref().to_owned());
    }

    let items: Vec<rs::InputContent> = parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => rs::InputContent::InputText(rs::InputTextContent {
                text: text.as_ref().to_owned(),
            }),
            ContentPart::Image { url } => rs::InputContent::InputImage(rs::InputImageContent {
                image_url: Some(url.as_ref().to_owned()),
                file_id: None,
                detail: rs::ImageDetail::default(),
            }),
        })
        .collect();

    rs::EasyInputContent::ContentList(items)
}

/// The request's client function tools. A function tool whose name collides with a backend-hosted
/// tool is dropped, because sending both is rejected as a duplicate, so the hosted tool wins.
///
/// No hosted tool is emitted here. Both ride the raw-JSON [`extra_tool_entries`] channel instead.
fn build_responses_tools(req: &ConversationRequest) -> Vec<rs::Tool> {
    let tools: Vec<rs::Tool> = req
        .tools
        .iter()
        .filter(|t| {
            let collides = req.hosted_tools.iter().any(|h| {
                h.wire_name() == t.name
                    || h.client_custom_name() == Some(t.name.as_str())
            });
            if collides {
                tracing::warn!(
                    tool = %t.name,
                    "dropping function tool that collides with a backend-hosted tool"
                );
            }
            !collides
        })
        .map(|t| {
            rs::Tool::Function(rs::FunctionTool {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: Some(t.parameters.clone()),
                strict: None,
            })
        })
        .collect();

    tools
}

/// Every hosted tool as a raw JSON entry, which the sampler client splices into the serialized
/// `tools` array. `x_search` rides this channel because it has no `rs::Tool` variant, and
/// `web_search` rides it because async_openai's `rs::WebSearchToolFilters` models only
/// `allowed_domains` and cannot carry `excluded_domains`. Emitting either as a typed `rs::Tool`
/// as well would send it twice, which the API rejects as a duplicate; the JSON built here is
/// byte-identical to the native `rs::Tool::WebSearch` for the no-filter and allowlist-only cases.
///
/// `profile` is the transport identity of the client that will send this
/// request. xAI's backend-hosted search tools (`x_search`/`web_search`) exist
/// only on the xAI backend, so any non-xAI profile drops them here — at the
/// single serialization chokepoint — rather than trusting every upstream
/// caller (catalog data, `[model.*]` overrides, subagent-inherited cells) to
/// have kept `supports_backend_search` false. [`HostedTool::ClientCustom`] is
/// provider-neutral (a client-executed tool such as Code Mode's `exec`) and is
/// always kept.
pub fn extra_tool_entries(
    hosted_tools: &[HostedTool],
    profile: ProviderProfile,
) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    for tool in hosted_tools {
        match tool {
            HostedTool::WebSearch { options } => {
                if drop_hosted_search_for_profile(profile, tool.wire_name()) {
                    continue;
                }
                entries.push(match options {
                    Some(o) => o.to_tool_entry(),
                    None => WebSearchOptions::default().to_tool_entry(),
                });
            }
            HostedTool::XSearch { options } => {
                if drop_hosted_search_for_profile(profile, tool.wire_name()) {
                    continue;
                }
                entries.push(match options {
                    Some(o) => o.to_tool_entry(),
                    None => XSearchOptions::default().to_tool_entry(),
                });
            }
            HostedTool::ClientCustom(tool) => {
                let mut entry = serde_json::json!({
                    "type": "custom",
                    "name": tool.name,
                    "format": serde_json::to_value(&tool.format)
                        .expect("custom tool format must serialize"),
                });
                if let Some(description) = &tool.description {
                    entry["description"] = serde_json::Value::String(description.clone());
                }
                entries.push(entry);
            }
        }
    }
    entries
}

/// Provider gate for xAI backend-hosted search tools. Returns `true` (drop)
/// for every non-xAI provider; warns once per process so a misconfigured
/// catalog or override doesn't spam the log on every request.
fn drop_hosted_search_for_profile(profile: ProviderProfile, wire_name: &'static str) -> bool {
    if matches!(profile.provider, crate::types::ModelProvider::Xai) {
        return false;
    }
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            tool = wire_name,
            provider = ?profile.provider,
            "dropping xAI backend-hosted search tool: it does not exist on this \
             provider's backend and must not appear in its request body"
        );
    });
    true
}
