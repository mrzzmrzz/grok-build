//! Layer-2 stream transform for the OpenAI Responses API.
//!
//! Consumes a raw `rs::ResponseStreamEvent` stream and produces
//! [`SamplingEvent`]s. Pure: no I/O, no shell coupling.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};

use xai_grok_sampling_types::{
    ConversationItem, ConversationResponse, ResponseModelMetadata, SamplingError, StopReason,
    TokenUsage, rs,
};

use crate::doom_loop_recovery::FailedResponseCapture;
use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

/// Returns whether a Responses API event reflects real model progress
/// rather than a liveness-only heartbeat / status transition.
pub(crate) fn responses_event_has_meaningful_content(event: &rs::ResponseStreamEvent) -> bool {
    use rs::ResponseStreamEvent;

    match event {
        ResponseStreamEvent::ResponseCreated(_)
        | ResponseStreamEvent::ResponseInProgress(_)
        | ResponseStreamEvent::ResponseQueued(_) => false,
        ResponseStreamEvent::ResponseOutputTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseOutputTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseRefusalDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseRefusalDone(event) => !event.refusal.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDone(event) => {
            !event.arguments.is_empty() || event.name.as_ref().is_some_and(|name| !name.is_empty())
        }
        ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningSummaryTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDone(event) => !event.arguments.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDone(event) => !event.code.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDone(event) => !event.input.is_empty(),
        ResponseStreamEvent::ResponseFailed(event) => {
            !event.response.output.is_empty()
                || event
                    .response
                    .usage
                    .as_ref()
                    .is_some_and(|usage| usage.output_tokens > 0)
        }
        ResponseStreamEvent::ResponseCompleted(_)
        | ResponseStreamEvent::ResponseIncomplete(_)
        | ResponseStreamEvent::ResponseOutputItemAdded(_)
        | ResponseStreamEvent::ResponseOutputItemDone(_)
        | ResponseStreamEvent::ResponseContentPartAdded(_)
        | ResponseStreamEvent::ResponseContentPartDone(_)
        | ResponseStreamEvent::ResponseFileSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseFileSearchCallSearching(_)
        | ResponseStreamEvent::ResponseFileSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseWebSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseWebSearchCallSearching(_)
        | ResponseStreamEvent::ResponseWebSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartAdded(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartDone(_)
        | ResponseStreamEvent::ResponseImageGenerationCallCompleted(_)
        | ResponseStreamEvent::ResponseImageGenerationCallGenerating(_)
        | ResponseStreamEvent::ResponseImageGenerationCallInProgress(_)
        | ResponseStreamEvent::ResponseImageGenerationCallPartialImage(_)
        | ResponseStreamEvent::ResponseMCPCallCompleted(_)
        | ResponseStreamEvent::ResponseMCPCallFailed(_)
        | ResponseStreamEvent::ResponseMCPCallInProgress(_)
        | ResponseStreamEvent::ResponseMCPListToolsCompleted(_)
        | ResponseStreamEvent::ResponseMCPListToolsFailed(_)
        | ResponseStreamEvent::ResponseMCPListToolsInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInterpreting(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallCompleted(_)
        | ResponseStreamEvent::ResponseOutputTextAnnotationAdded(_)
        | ResponseStreamEvent::ResponseError(_) => true,
    }
}

pub(crate) fn responses_event_may_have_output(event: &rs::ResponseStreamEvent) -> bool {
    !matches!(event, rs::ResponseStreamEvent::ResponseError(_))
        && responses_event_has_meaningful_content(event)
}

/// Whether a mid-stream deserialization failure names an event *kind* the
/// typed enum does not know, as opposed to a known event whose payload is
/// malformed.
///
/// The provider may add future side-channel events at any time; those must
/// be skipped without failing the request (SPEC §9.4). A known event that
/// fails to parse is a real wire-contract violation and stays a hard error —
/// silently dropping it would corrupt the turn. The SSE decoder reports both
/// as `SamplingError::Serialization`, so the split happens here on serde's
/// stable unknown-variant message: only the top-level
/// `ResponseStreamEvent` tag lists `response.created` among its expected
/// variants, which keeps an unknown *nested* discriminator (a new output-item
/// type inside a known event) on the hard-error path.
fn is_unknown_response_event_kind(err: &SamplingError) -> bool {
    let SamplingError::Serialization(serde_err) = err else {
        return false;
    };
    is_unknown_response_event_kind_message(&serde_err.to_string())
}

/// Message-level core of [`is_unknown_response_event_kind`], shared with the
/// SSE decoder (`client::deserialize_response_event`) so both layers classify
/// an unknown top-level event kind identically — the decoder to pick its log
/// level, this layer to skip the frame.
pub(crate) fn is_unknown_response_event_kind_message(message: &str) -> bool {
    message.starts_with("unknown variant") && message.contains("`response.created`")
}

fn missing_tool_input_suffix(streamed: &mut String, complete: &str) -> Option<String> {
    let suffix = complete.strip_prefix(streamed.as_str())?;
    if suffix.is_empty() {
        return None;
    }
    let suffix = suffix.to_owned();
    streamed.push_str(&suffix);
    Some(suffix)
}

/// Copy everything the Doom-loop capture needs out of a frame.
///
/// This is the single observation point: it runs for every frame *before* the
/// abort gate, so the frame a confident signal aborts on is observed exactly
/// like any other. Two things matter — a completed item is the authoritative
/// copy of what the deltas approximated, and any frame that names tool
/// activity or compaction state vetoes the replay, since reasoning must never
/// be retried without the item it is bound to.
fn observe_for_recovery(capture: &FailedResponseCapture, event: &rs::ResponseStreamEvent) {
    use rs::ResponseStreamEvent as Event;
    if !capture.is_armed() {
        return;
    }
    match event {
        Event::ResponseOutputTextDelta(text) => capture.record_output_delta(
            text.output_index,
            text.content_index,
            text.item_id.clone(),
            &text.delta,
        ),
        Event::ResponseOutputTextDone(text) => capture.record_output_done(
            text.output_index,
            text.content_index,
            text.item_id.clone(),
            text.text.clone(),
        ),
        Event::ResponseReasoningTextDelta(reasoning) => capture.record_reasoning_delta(
            reasoning.output_index,
            reasoning.content_index,
            reasoning.item_id.clone(),
            &reasoning.delta,
        ),
        Event::ResponseReasoningTextDone(reasoning) => capture.record_reasoning_done(
            reasoning.output_index,
            reasoning.content_index,
            reasoning.item_id.clone(),
            reasoning.text.clone(),
        ),
        Event::ResponseReasoningSummaryTextDelta(summary) => capture
            .record_reasoning_summary_delta(
                summary.output_index,
                summary.summary_index,
                summary.item_id.clone(),
                &summary.delta,
            ),
        Event::ResponseReasoningSummaryTextDone(summary) => capture.record_reasoning_summary_done(
            summary.output_index,
            summary.summary_index,
            summary.item_id.clone(),
            summary.text.clone(),
        ),
        Event::ResponseOutputItemAdded(added) => capture.record_item_start(&added.item),
        Event::ResponseOutputItemDone(done) => {
            capture.record_output_item(done.output_index, &done.item);
        }
        Event::ResponseCompleted(completed) => {
            capture.record_terminal_output(&completed.response.output);
        }
        Event::ResponseIncomplete(incomplete) => {
            capture.record_terminal_output(&incomplete.response.output);
        }
        // Frames that only name in-flight tool work. The item they belong to
        // may never complete on this attempt, so the frame itself is the
        // notice that a call was in flight.
        Event::ResponseFunctionCallArgumentsDelta(_)
        | Event::ResponseFunctionCallArgumentsDone(_)
        | Event::ResponseCustomToolCallInputDelta(_)
        | Event::ResponseCustomToolCallInputDone(_)
        | Event::ResponseCodeInterpreterCallCodeDelta(_)
        | Event::ResponseCodeInterpreterCallCodeDone(_)
        | Event::ResponseCodeInterpreterCallInProgress(_)
        | Event::ResponseCodeInterpreterCallInterpreting(_)
        | Event::ResponseCodeInterpreterCallCompleted(_)
        | Event::ResponseFileSearchCallInProgress(_)
        | Event::ResponseFileSearchCallSearching(_)
        | Event::ResponseFileSearchCallCompleted(_)
        | Event::ResponseWebSearchCallInProgress(_)
        | Event::ResponseWebSearchCallSearching(_)
        | Event::ResponseWebSearchCallCompleted(_)
        | Event::ResponseImageGenerationCallInProgress(_)
        | Event::ResponseImageGenerationCallGenerating(_)
        | Event::ResponseImageGenerationCallCompleted(_)
        | Event::ResponseMCPCallInProgress(_)
        | Event::ResponseMCPCallCompleted(_)
        | Event::ResponseMCPCallFailed(_)
        | Event::ResponseMCPCallArgumentsDelta(_)
        | Event::ResponseMCPCallArgumentsDone(_) => capture.record_unreplayable(),
        _ => {}
    }
}

/// Transform a raw Responses API event stream into a stream of
/// [`SamplingEvent`]s.
///
/// Yields exactly one terminal event ([`SamplingEvent::Completed`] or
/// [`SamplingEvent::Failed`]) per request. Server-side `ResponseFailed`
/// and `ResponseError` events are translated to
/// `SamplingError::Api { status: 500, .. }` so the actor's retry loop
/// treats them as retryable.
///
/// `doom_loop` is the collector returned alongside `raw_stream` by
/// `SamplingClient::conversation_stream_responses`; any signals the SSE
/// decoder recorded are drained onto the final `ConversationResponse`.
/// `None` (check disabled) leaves the response untouched.
pub fn stream_responses<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    stream_responses_with_client_custom_tools(
        raw_stream,
        model_metadata,
        request_id,
        idle_timeout,
        doom_loop,
        Vec::new(),
    )
}

/// Responses stream transform with the names of client-executed native
/// custom tools. An unlisted `CustomToolCall` remains the xAI hosted-search
/// carrier used by the official grok backend.
pub fn stream_responses_with_client_custom_tools<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
    client_custom_tool_names: Vec<String>,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    stream_responses_tracked_with_client_custom_tools(
        raw_stream,
        model_metadata,
        request_id,
        idle_timeout,
        doom_loop,
        Arc::new(AtomicBool::new(false)),
        FailedResponseCapture::default(),
        client_custom_tool_names,
    )
}

pub(crate) fn stream_responses_tracked<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
    output_observed: Arc<AtomicBool>,
    failed_response: FailedResponseCapture,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    stream_responses_tracked_with_client_custom_tools(
        raw_stream,
        model_metadata,
        request_id,
        idle_timeout,
        doom_loop,
        output_observed,
        failed_response,
        Vec::new(),
    )
}

pub(crate) fn stream_responses_tracked_with_client_custom_tools<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
    output_observed: Arc<AtomicBool>,
    failed_response: FailedResponseCapture,
    client_custom_tool_names: Vec<String>,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    async_stream::stream! {
        use rs::{ResponseStreamEvent, Status};

        let stream_start = Instant::now();
        let mut chunk_timestamps: Vec<Instant> = Vec::new();

        yield SamplingEvent::StreamStarted {
            request_id: request_id.clone(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        if let Some(metadata) = model_metadata {
            yield SamplingEvent::ModelMetadata {
                request_id: request_id.clone(),
                metadata,
            };
        }

        let mut final_response: Option<rs::Response> = None;
        // Durable copy of every completed output item, keyed by
        // `output_index` so a re-emitted done frame overwrites rather than
        // duplicates. Discarded when a terminal response arrives (that stays
        // authoritative); used to rebuild the turn when the stream dies
        // before one. Local to this attempt — independent of the doom-loop
        // capture, which serves the retry path, not this stream's output.
        let mut durable_output: BTreeMap<u32, rs::OutputItem> = BTreeMap::new();
        // The `ResponseCreated` envelope, kept as the shell (model id,
        // response id, ...) for a durable-output recovery.
        let mut created_response: Option<rs::Response> = None;
        let mut chunk_index: u64 = 0;
        // (item_id, summary_index) of the last reasoning-summary delta, for
        // paragraph breaks at part boundaries.
        let mut last_summary_part: Option<(String, u32)> = None;
        let mut message_chunk_count: u64 = 0;
        let mut first_token_emitted = false;
        let mut reasoning_acc = String::new();
        let mut last_content_chunk_at = Instant::now();

        // Maps Responses API `output_index` to our tool-only `tool_index`.
        // Populated when `ResponseOutputItemAdded` carries a `FunctionCall`;
        // later `ResponseFunctionCallArgumentsDelta` events
        // look up `output_index` here to find the matching `tool_index`.
        let mut output_to_tool_index: BTreeMap<u32, u32> = BTreeMap::new();
        let mut custom_input_streamed: BTreeMap<u32, String> = BTreeMap::new();
        let mut arguments_complete_emitted: BTreeSet<u32> = BTreeSet::new();
        let mut next_tool_index: u32 = 0;

        let mut stream = raw_stream;
        // A stream death (idle timeout / retryable transport error) after at
        // least one complete output item takes the same durable-output
        // recovery as a clean EOF — set instead of yielding Failed so the
        // completed items are not discarded and re-billed by a retry.
        let recover_durable = |durable_output: &BTreeMap<u32, rs::OutputItem>,
                                   err: &SamplingError,
                                   retryable_only: bool| {
            if durable_output.is_empty() || (retryable_only && !err.is_retryable()) {
                return false;
            }
            tracing::warn!(
                request_id = %request_id,
                error = %err,
                recovered_items = durable_output.len(),
                "stream died after completed output items; recovering durable output"
            );
            true
        };
        loop {
            let event_result = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(event_result)) => event_result,
                Ok(None) => break,
                Err(_elapsed) => {
                    let err = SamplingError::IdleTimeout {
                        elapsed_secs: idle_timeout.as_secs(),
                    };
                    if recover_durable(&durable_output, &err, false) {
                        break;
                    }
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            let event = match event_result {
                Ok(event) => event,
                // A future side-channel event kind the typed enum does not
                // know: ignore it and keep the stream alive. It counts as
                // liveness only, not content, so the content-aware idle
                // check still applies before moving on.
                Err(err) if is_unknown_response_event_kind(&err) => {
                    tracing::debug!(
                        request_id = %request_id,
                        error = %err,
                        "ignoring unknown Responses stream event kind"
                    );
                    if last_content_chunk_at.elapsed() > idle_timeout {
                        let err = SamplingError::IdleTimeout {
                            elapsed_secs: idle_timeout.as_secs(),
                        };
                        if recover_durable(&durable_output, &err, false) {
                            break;
                        }
                        yield SamplingEvent::Failed {
                            request_id: request_id.clone(),
                            error: SamplingErrorInfo::from(&err),
                        };
                        return;
                    }
                    continue;
                }
                // Everything else — transport failures and known events with
                // malformed payloads alike — fails the attempt loudly. A
                // retryable transport failure after complete output items
                // instead takes the durable recovery: a retry would discard
                // that finished work. Non-retryable failures (a known event
                // with a malformed payload is a wire-contract violation)
                // stay hard errors.
                Err(err) => {
                    if recover_durable(&durable_output, &err, true) {
                        break;
                    }
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            if responses_event_may_have_output(&event) {
                output_observed.store(true, Ordering::Relaxed);
            }

            // A confident midstream signal aborts the attempt immediately.
            // Terminal frames are processed so their complete response items
            // remain available to the retry loop; `drive_l2` rejects the
            // completed response before it can be accepted.
            let is_terminal_response = matches!(
                &event,
                ResponseStreamEvent::ResponseCompleted(_)
                    | ResponseStreamEvent::ResponseIncomplete(_)
            );
            // Observed before the abort gate so the aborting frame lands in
            // the capture like any other; the attempt is discarded either
            // way, so nothing here is surfaced downstream.
            observe_for_recovery(&failed_response, &event);

            if !is_terminal_response
                && let Some(triggers) = doom_loop.as_ref().and_then(|c| c.abort_triggers())
            {
                let err = SamplingError::DoomLoopDetected {
                    triggers,
                    aborted_at_chunk: Some(chunk_index),
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            let event_has_content = responses_event_has_meaningful_content(&event);

            // Track whether ResponseIncomplete should break the loop
            // after the content-aware idle check below.
            let mut should_break = false;

            match event {
                ResponseStreamEvent::ResponseOutputTextDelta(text_delta_event) => {
                    let delta = text_delta_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_timestamps.push(Instant::now());
                        chunk_index += 1;
                        message_chunk_count += 1;
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Text,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseReasoningSummaryTextDelta(summary_event) => {
                    let delta = summary_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        // A reasoning item carries multiple summary parts
                        // (distinct summary_index, and Codex regularly sends
                        // several per item). The parts are separate paragraphs;
                        // concatenating their deltas verbatim fuses them into
                        // one line ("**A****B**"), so a part boundary emits a
                        // paragraph break into the thought stream first.
                        let part = (
                            summary_event.item_id.clone(),
                            summary_event.summary_index,
                        );
                        let crossed_boundary =
                            last_summary_part.as_ref().is_some_and(|prev| prev != &part);
                        last_summary_part = Some(part);
                        if crossed_boundary {
                            chunk_index += 1;
                            yield SamplingEvent::ChannelToken {
                                request_id: request_id.clone(),
                                channel: SamplingChannel::Reasoning,
                                text: "\n\n".to_string(),
                                chunk_index,
                            };
                        }
                        chunk_index += 1;
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseReasoningTextDelta(reasoning_event) => {
                    let delta = reasoning_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        reasoning_acc.push_str(&delta);
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                // Start of a Responses FunctionCall — emit initial id+name
                // and remember the output_index → tool_index mapping.
                ResponseStreamEvent::ResponseOutputItemAdded(added_event) => {
                    match added_event.item {
                        rs::OutputItem::FunctionCall(fc) => {
                            let tool_index = next_tool_index;
                            next_tool_index += 1;
                            output_to_tool_index.insert(added_event.output_index, tool_index);

                            yield SamplingEvent::ToolCallDelta {
                                request_id: request_id.clone(),
                                tool_index,
                                id: Some(fc.call_id),
                                name: Some(fc.name),
                                arguments_delta: None,
                            };
                        }
                        rs::OutputItem::CustomToolCall(ct)
                            if client_custom_tool_names.iter().any(|name| name == &ct.name) =>
                        {
                            let tool_index = next_tool_index;
                            next_tool_index += 1;
                            output_to_tool_index.insert(added_event.output_index, tool_index);
                            custom_input_streamed
                                .insert(added_event.output_index, ct.input.clone());
                            let call = xai_grok_sampling_types::ToolCall::custom(
                                &ct.call_id,
                                &ct.id,
                                &ct.name,
                                ct.input.clone(),
                            );

                            yield SamplingEvent::ToolCallDelta {
                                request_id: request_id.clone(),
                                tool_index,
                                id: Some(call.id.as_ref().to_owned()),
                                name: Some(ct.name),
                                arguments_delta: (!ct.input.is_empty()).then_some(ct.input),
                            };
                        }
                        _ => {}
                    }
                }

                // Continuation chunk for a streaming FunctionCall's args.
                // Drop silently if no preceding OutputItemAdded mapped.
                ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(args_event) => {
                    let delta = args_event.delta;
                    if !delta.is_empty()
                        && let Some(&tool_index) =
                            output_to_tool_index.get(&args_event.output_index)
                    {
                        yield SamplingEvent::ToolCallDelta {
                            request_id: request_id.clone(),
                            tool_index,
                            id: None,
                            name: None,
                            arguments_delta: Some(delta),
                        };
                    }
                }

                // Native custom tools stream raw text rather than JSON args.
                ResponseStreamEvent::ResponseCustomToolCallInputDelta(input_event) => {
                    let delta = input_event.delta;
                    if !delta.is_empty()
                        && !arguments_complete_emitted.contains(&input_event.output_index)
                        && let Some(&tool_index) =
                            output_to_tool_index.get(&input_event.output_index)
                    {
                        custom_input_streamed
                            .entry(input_event.output_index)
                            .or_default()
                            .push_str(&delta);
                        yield SamplingEvent::ToolCallDelta {
                            request_id: request_id.clone(),
                            tool_index,
                            id: None,
                            name: None,
                            arguments_delta: Some(delta),
                        };
                    }
                }

                ResponseStreamEvent::ResponseCompleted(completed_event) => {
                    final_response = Some(completed_event.response);
                }

                ResponseStreamEvent::ResponseIncomplete(incomplete_event) => {
                    final_response = Some(incomplete_event.response);
                    should_break = true;
                }

                ResponseStreamEvent::ResponseFailed(failed_event) => {
                    let response = failed_event.response;
                    let error_message = response
                        .error
                        .as_ref()
                        .map(|e| format!("{}: {}", e.code, e.message))
                        .unwrap_or_else(|| "Response failed with unknown error".to_string());
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message,
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                        error_code: response
                            .error
                            .as_ref()
                            .map(|e| xai_grok_sampling_types::ApiErrorCode::parse(&e.code)),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                ResponseStreamEvent::ResponseError(error_event) => {
                    let error_message = format!(
                        "{}: {}",
                        error_event.code.as_deref().unwrap_or("error"),
                        error_event.message
                    );
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message,
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                        // The wire code, absent when the event carried none.
                        error_code: error_event
                            .code
                            .as_deref()
                            .map(xai_grok_sampling_types::ApiErrorCode::parse),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                // ── Backend-hosted tool lifecycle events ────────────
                // These tools are executed server-side by the agentic
                // sampler. We emit progress events so the shell/pager
                // can show status to the user.

                // Web search
                ResponseStreamEvent::ResponseWebSearchCallInProgress(ev) => {
                    yield SamplingEvent::BackendToolCallStarted {
                        request_id: request_id.clone(),
                        call_id: ev.item_id.clone(),
                        name: "web_search".to_string(),
                    };
                }
                // Completed/Searching carry no data — the real payload
                // arrives via ResponseOutputItemDone(WebSearchCall) below.
                ResponseStreamEvent::ResponseWebSearchCallCompleted(_)
                | ResponseStreamEvent::ResponseWebSearchCallSearching(_) => {}

                // Code interpreter (server-side, like web/x search). Surface it
                // the same way x_search is: a generic backend tool call that the
                // shell renders as a client `tool_use` + `user` `tool_result`
                // split (grok has no HostedTool::CodeInterpreter, so these events
                // are latent under the current hosted-tool set). The started
                // event fires on InProgress; the full payload (code + outputs)
                // rides ResponseOutputItemDone(CodeInterpreterCall) below.
                ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(ev) => {
                    yield SamplingEvent::BackendToolCallStarted {
                        request_id: request_id.clone(),
                        call_id: ev.item_id.clone(),
                        name: "code_interpreter".to_string(),
                    };
                }
                // Interpreting/Completed carry no payload — the result arrives
                // via ResponseOutputItemDone(CodeInterpreterCall) below.
                ResponseStreamEvent::ResponseCodeInterpreterCallInterpreting(_)
                | ResponseStreamEvent::ResponseCodeInterpreterCallCompleted(_) => {}

                // OutputItemDone carries the full result for backend tools.
                // For WebSearchCall this includes the query and source URLs.
                // For CustomToolCall this includes x_search results.
                ResponseStreamEvent::ResponseOutputItemDone(done_event) => {
                    match &done_event.item {
                        rs::OutputItem::WebSearchCall(ws) => {
                            let result = serde_json::to_value(ws).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ws.id.clone(),
                                name: "web_search".to_string(),
                                result,
                            };
                        }
                        // X search results arrive as CustomToolCall with
                        // names like x_keyword_search, x_semantic_search, etc.
                        // Use "x_search" consistently (matching the Started event);
                        // the specific sub-type is in the serialized result payload
                        // and extracted by the pager from raw_output.name.
                        rs::OutputItem::CustomToolCall(ct)
                            if client_custom_tool_names.iter().any(|name| name == &ct.name) =>
                        {
                            if let Some(&tool_index) =
                                output_to_tool_index.get(&done_event.output_index)
                                && arguments_complete_emitted.insert(done_event.output_index)
                            {
                                let streamed = custom_input_streamed
                                    .entry(done_event.output_index)
                                    .or_default();
                                if let Some(arguments_delta) =
                                    missing_tool_input_suffix(streamed, &ct.input)
                                {
                                    yield SamplingEvent::ToolCallDelta {
                                        request_id: request_id.clone(),
                                        tool_index,
                                        id: None,
                                        name: None,
                                        arguments_delta: Some(arguments_delta),
                                    };
                                }
                                yield SamplingEvent::ToolCallArgumentsComplete {
                                    request_id: request_id.clone(),
                                    tool_index,
                                    id: Some(ct.call_id.clone()),
                                    name: Some(ct.name.clone()),
                                };
                            }
                        }
                        rs::OutputItem::CustomToolCall(ct) => {
                            let result = serde_json::to_value(ct).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ct.id.clone(),
                                name: "x_search".to_string(),
                                result,
                            };
                        }
                        // Code interpreter: the full call (code + outputs) rides
                        // the done item. Surfaced under the shared "code_interpreter"
                        // name (matching the Started event); the shell renders it via
                        // the client `tool_use` + `user` `tool_result` split.
                        rs::OutputItem::CodeInterpreterCall(ci) => {
                            let result = serde_json::to_value(ci).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ci.id.clone(),
                                name: "code_interpreter".to_string(),
                                result,
                            };
                        }
                        _ => {}
                    }
                    durable_output.insert(done_event.output_index, done_event.item);
                }

                ResponseStreamEvent::ResponseCreated(created_event) => {
                    if created_response.is_none() {
                        created_response = Some(created_event.response);
                    }
                }

                // A done event without a preceding delta still carries the
                // complete custom input. Unlisted custom calls remain hosted
                // x_search and use the backend lifecycle below.
                ResponseStreamEvent::ResponseCustomToolCallInputDone(ev) => {
                    if let Some(&tool_index) = output_to_tool_index.get(&ev.output_index) {
                        if !arguments_complete_emitted.contains(&ev.output_index) {
                            let streamed = custom_input_streamed
                                .entry(ev.output_index)
                                .or_default();
                            if let Some(arguments_delta) =
                                missing_tool_input_suffix(streamed, &ev.input)
                            {
                                yield SamplingEvent::ToolCallDelta {
                                    request_id: request_id.clone(),
                                    tool_index,
                                    id: None,
                                    name: None,
                                    arguments_delta: Some(arguments_delta),
                                };
                            }
                            arguments_complete_emitted.insert(ev.output_index);
                            yield SamplingEvent::ToolCallArgumentsComplete {
                                request_id: request_id.clone(),
                                tool_index,
                                id: None,
                                name: None,
                            };
                        }
                    } else {
                        yield SamplingEvent::BackendToolCallStarted {
                            request_id: request_id.clone(),
                            call_id: ev.item_id.clone(),
                            name: "x_search".to_string(),
                        };
                    }
                }

                // All other *known* events (intermediate progress,
                // annotations, image gen, file search, etc.) — parsed fine,
                // no action needed. Unknown event kinds never reach this
                // match: they are skipped at the deserialization boundary
                // above, while known-but-malformed payloads fail the attempt
                // there instead of being silently dropped here.
                _ => {}
            }

            if event_has_content {
                last_content_chunk_at = Instant::now();
            } else if last_content_chunk_at.elapsed() > idle_timeout {
                let err = SamplingError::IdleTimeout {
                    elapsed_secs: idle_timeout.as_secs(),
                };
                if recover_durable(&durable_output, &err, false) {
                    break;
                }
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            if should_break {
                break;
            }
        }

        // ── Build the final response ─────────────────────────────────
        let mut response = match final_response {
            // The terminal response is authoritative *for its own content*.
            // The ChatGPT/Codex dialect terminates with `response.completed`
            // carrying `"output": []` — the turn's items only ever arrive on
            // `response.output_item.done` — so an empty terminal projection
            // over completed items is a dialect difference, not an empty
            // turn. Splicing the durable copies back in is the only way that
            // turn survives; discarding them made every Codex reply read as
            // an empty response and drove the retry loop to resample the
            // same turn until the user cancelled.
            Some(mut r) if r.output.is_empty() && !durable_output.is_empty() => {
                tracing::debug!(
                    request_id = %request_id,
                    recovered_items = durable_output.len(),
                    "terminal response carried no output; \
                     restoring the completed output items"
                );
                r.output = std::mem::take(&mut durable_output).into_values().collect();
                r
            }
            Some(r) => r,
            // The stream died before a terminal event but whole output items
            // completed: rebuild the turn from them instead of discarding
            // everything. Marked Incomplete — the tail may be missing.
            None if !durable_output.is_empty() => {
                tracing::warn!(
                    request_id = %request_id,
                    recovered_items = durable_output.len(),
                    "stream ended without ResponseCompleted/ResponseIncomplete; \
                     recovering from completed output items"
                );
                let mut recovered = created_response
                    .take()
                    .unwrap_or_else(recovery_response_shell);
                // Every recovered item is complete by construction (only
                // `ResponseOutputItemDone` frames land here). When the last
                // one is the assistant message, the turn's final item
                // finished before the stream died: surface it as a terminal
                // completion instead of reclassifying it as a max-token
                // truncation. Anything else (e.g. reasoning with no
                // message) may be missing its tail and stays Incomplete.
                let message_completed_last = durable_output
                    .values()
                    .next_back()
                    .is_some_and(|item| matches!(item, rs::OutputItem::Message(_)));
                recovered.status = if message_completed_last {
                    Status::Completed
                } else {
                    Status::Incomplete
                };
                recovered.output = std::mem::take(&mut durable_output).into_values().collect();
                recovered
            }
            None => {
                let err = SamplingError::Api {
                    status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    message: "No ResponseCompleted or ResponseIncomplete event received from \
                              Responses API"
                        .to_string(),
                    model_metadata: None,
                    retry_after_secs: None,
                    should_retry: None,
                    // Synthesized client-side; no wire envelope to read.
                    error_code: None,
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }
        };

        // Billing fields (`prompt_tokens`, `completion_tokens`,
        // `cached_prompt_tokens`, `reasoning_tokens`) are the cumulative
        // wire values — they sum across every server-side turn of the
        // agent loop and are what we bill on / log to telemetry.
        //
        // `total_tokens` is the live context length used to drive the
        // CLI `/context` bar, the auto-compact threshold, and
        // `meta.totalTokens` on persisted sessions. The SSE decoder
        // (`deserialize_response_event`) has already rewritten
        // `u.total_tokens` to `context_details.input + output` when
        // the backend emits it; on older deployments the wire
        // value passes through unchanged.
        let usage = response.usage.as_ref().map(|u| TokenUsage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
            reasoning_tokens: u.output_tokens_details.reasoning_tokens,
            cached_prompt_tokens: u.input_tokens_details.cached_tokens,
            cache_creation_prompt_tokens: 0,
        });

        let cost_usd_ticks = response
            .metadata
            .as_mut()
            .and_then(|m| m.remove(crate::client::COST_USD_TICKS_METADATA_KEY))
            .and_then(|s| s.parse::<i64>().ok());

        let status = response.status.clone();

        // Convert to ConversationItem(s); patch in accumulated reasoning
        // text as a fallback when the final response lacks `content` /
        // `summary` (the streaming deltas may have arrived out of band).
        // Splice policy lives in `inject_streaming_reasoning_fallback`.
        let mut items =
            xai_grok_sampling_types::response_to_conversation_items_with_client_custom_tools(
                response,
                &client_custom_tool_names,
            );
        xai_grok_sampling_types::inject_streaming_reasoning_fallback(&mut items, reasoning_acc);

        let has_tool_calls = items.iter().any(|i| match i {
            ConversationItem::Assistant(a) => !a.tool_calls.is_empty(),
            _ => false,
        });

        let stop_reason = if has_tool_calls {
            Some(StopReason::ToolCalls)
        } else {
            match status {
                Status::Completed => Some(StopReason::Stop),
                Status::Incomplete => Some(StopReason::Length),
                _ => None,
            }
        };

        let stream_end = Instant::now();
        let metrics =
            InferenceLatencyStats::from_timestamps(stream_start, &chunk_timestamps, stream_end);

        // Warn-only for now: surface the server-reported triggers once per
        // request (raw labels only — ZDR-safe) and attach them for callers.
        let doom_loop_signals = doom_loop
            .as_ref()
            .map(|collector| collector.take())
            .unwrap_or_default();
        if !doom_loop_signals.is_empty() {
            tracing::warn!(
                request_id = %request_id,
                triggers = ?doom_loop_signals.iter().map(|s| s.raw.as_str()).collect::<Vec<_>>(),
                "server reported doom-loop triggers for this response"
            );
        }

        let conversation_response = ConversationResponse {
            items,
            stop_reason,
            usage,
            cost_usd_ticks,
            message_chunks_emitted: message_chunk_count,
            doom_loop_signals,
            stop_message: None, // not reported on the Responses API
            message_id: None,   // no provider message id on the Responses API
            raw_stop_reason: None,
            stop_sequence: None,
        };

        yield SamplingEvent::Completed {
            request_id: request_id.clone(),
            response: Box::new(conversation_response),
            metrics,
        };
    }
}

/// Envelope for a durable-output recovery when the stream died before even
/// `ResponseCreated` supplied one.
fn recovery_response_shell() -> rs::Response {
    rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 0,
        completed_at: None,
        error: None,
        id: String::new(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: String::new(),
        object: "response".into(),
        output: Vec::new(),
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Incomplete,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses as rs_types;
    use futures_util::stream;
    use std::pin::pin;

    fn rid() -> RequestId {
        RequestId::from("resp-test")
    }

    /// Build a minimal `rs_types::Response` for use in `ResponseCompleted`
    fn build_response(status: rs_types::Status) -> rs_types::Response {
        rs_types::Response {
            background: None,
            billing: None,
            conversation: None,
            created_at: 0,
            completed_at: None,
            error: None,
            id: "resp_1".into(),
            incomplete_details: None,
            instructions: None,
            max_output_tokens: None,
            metadata: None,
            model: "test-model".into(),
            object: "response".into(),
            output: vec![],
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None,
            service_tier: None,
            status,
            temperature: None,
            text: None,
            tool_choice: None,
            tools: None,
            top_logprobs: None,
            top_p: None,
            truncation: None,
            usage: None,
        }
    }

    fn empty_completed_response() -> rs_types::Response {
        build_response(rs_types::Status::Completed)
    }

    fn failed_response_with_error(message: &str) -> rs_types::Response {
        let mut r = build_response(rs_types::Status::Failed);
        r.error = Some(rs_types::ErrorObject {
            code: "server_error".into(),
            message: message.into(),
        });
        r
    }

    fn text_delta_event(delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputTextDelta(rs_types::ResponseTextDeltaEvent {
            sequence_number: 0,
            item_id: "item-1".into(),
            output_index: 0,
            content_index: 0,
            delta: delta.into(),
            logprobs: None,
        })
    }

    fn completed_event() -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
            response: empty_completed_response(),
            sequence_number: 0,
        })
    }

    async fn collect(s: impl Stream<Item = SamplingEvent>) -> Vec<SamplingEvent> {
        let mut out = Vec::new();
        let mut s = pin!(s);
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
        out
    }

    /// A confident signal that aborts on a custom-tool input frame still
    /// vetoes the replay: the frame is the only notice that a call was in
    /// flight, and reasoning must never be retried without it. The same holds
    /// for the code-interpreter code frames.
    #[tokio::test]
    async fn an_abort_on_a_tool_input_frame_vetoes_the_replay() {
        for tool_frame in [
            rs::ResponseStreamEvent::ResponseCustomToolCallInputDelta(
                rs_types::ResponseCustomToolCallInputDeltaEvent {
                    sequence_number: 1,
                    output_index: 1,
                    item_id: "custom-1".into(),
                    delta: "{\"q\":".into(),
                },
            ),
            rs::ResponseStreamEvent::ResponseCodeInterpreterCallCodeDelta(
                rs_types::ResponseCodeInterpreterCallCodeDeltaEvent {
                    sequence_number: 1,
                    output_index: 1,
                    item_id: "ci-1".into(),
                    delta: "print(".into(),
                },
            ),
        ] {
            let capture = FailedResponseCapture::armed();
            // A collector that has already seen a confident trigger: the next
            // non-terminal frame aborts the attempt.
            let collector = crate::doom_loop::DoomLoopSignalCollector::new(
                xai_grok_sampling_types::DoomLoopRecoveryPolicy::default(),
            );
            collector.absorb(
                xai_grok_sampling_types::doom_loop::DOOM_LOOP_CHECK_EVENT_TYPE,
                r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking"]}}"#,
            );

            // Reasoning already captured, so an intact replay would carry it:
            // only the veto can empty the capture. The collector is armed
            // before the stream runs, so the abort lands on the tool frame.
            capture.record_reasoning_delta(0, 0, "reasoning-1".into(), "looping thought");
            let raw = stream::iter(vec![Ok(tool_frame), Ok(completed_event())]).boxed();
            let events = collect(stream_responses_tracked(
                raw,
                None,
                rid(),
                Duration::from_secs(60),
                Some(collector),
                Arc::new(AtomicBool::new(false)),
                capture.clone(),
            ))
            .await;

            assert!(
                matches!(events.last(), Some(SamplingEvent::Failed { .. })),
                "the confident signal aborts the attempt"
            );
            assert!(
                capture.take_items().is_empty(),
                "a turn with a call in flight replays nothing"
            );
        }
    }

    #[tokio::test]
    async fn missing_completed_event_yields_failed() {
        let raw =
            stream::iter(Vec::<Result<rs::ResponseStreamEvent, SamplingError>>::new()).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_delta_then_completed_yields_completed_with_stop() {
        let raw = stream::iter(vec![Ok(text_delta_event("hello")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let text_tokens: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                SamplingEvent::ChannelToken {
                    channel: SamplingChannel::Text,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_tokens, vec!["hello"]);

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn empty_failed_response_is_not_treated_as_output() {
        let event = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("boom"),
            sequence_number: 0,
        });
        assert!(!responses_event_may_have_output(&event));
    }

    #[tokio::test]
    async fn response_failed_yields_failed_500() {
        let failed = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("boom"),
            sequence_number: 0,
        });
        let raw = stream::iter(vec![Ok(failed)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
                assert!(error.message.contains("boom"));
                // The wire code passes through verbatim — dropping it here
                // would disable strip recovery for coded Responses failures.
                assert_eq!(
                    error.error_code,
                    Some(xai_grok_sampling_types::ApiErrorCode::Other(
                        "server_error".into()
                    ))
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A coded `error` event must carry its code into the Failed info —
    /// this is the whole mid-stream strip-recovery chain for the Responses
    /// backend (the synthesized 500 + code classifies as an image error).
    #[tokio::test]
    async fn response_error_event_carries_code_into_failed() {
        let error_event = rs::ResponseStreamEvent::ResponseError(rs_types::ResponseErrorEvent {
            sequence_number: 0,
            code: Some(xai_grok_sampling_types::INVALID_IMAGE_ERROR_CODE.into()),
            message: "could not decode image".into(),
            param: None,
        });
        let raw = stream::iter(vec![Ok(error_event)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(
                    error.error_code,
                    Some(xai_grok_sampling_types::ApiErrorCode::InvalidImage)
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mid_stream_transport_error_yields_failed() {
        let raw = stream::iter(vec![
            Ok(text_delta_event("hi")),
            Err(SamplingError::EventStreamError("conn reset".into())),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Failed { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_when_stream_stalls() {
        let raw = stream::iter(vec![Ok(text_delta_event("hi"))])
            .chain(stream::pending())
            .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_millis(100),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::IdleTimeout);
            }
            other => panic!("expected Failed(IdleTimeout), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_metadata_yielded_after_stream_started() {
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let metadata = ResponseModelMetadata {
            context_window: Some(8192),
            ..Default::default()
        };
        let events = collect(stream_responses(
            raw,
            Some(metadata),
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(matches!(events[0], SamplingEvent::StreamStarted { .. }));
        assert!(matches!(events[1], SamplingEvent::ModelMetadata { .. }));
    }

    #[test]
    fn meaningful_content_classifier_basics() {
        // Text delta with content is meaningful.
        let event = text_delta_event("foo");
        assert!(responses_event_has_meaningful_content(&event));
        // Empty text delta is not.
        let empty = text_delta_event("");
        assert!(!responses_event_has_meaningful_content(&empty));
        // Completed is meaningful (terminal).
        assert!(responses_event_has_meaningful_content(&completed_event()));
    }

    #[test]
    fn output_classifier_covers_non_forwarded_backend_events() {
        let queued = rs::ResponseStreamEvent::ResponseQueued(rs_types::ResponseQueuedEvent {
            sequence_number: 0,
            response: empty_completed_response(),
        });
        assert!(!responses_event_may_have_output(&queued));

        let response_error = rs::ResponseStreamEvent::ResponseError(rs_types::ResponseErrorEvent {
            sequence_number: 1,
            code: Some("server_error".into()),
            message: "failed before output".into(),
            param: None,
        });
        assert!(!responses_event_may_have_output(&response_error));

        let refusal =
            rs::ResponseStreamEvent::ResponseRefusalDelta(rs_types::ResponseRefusalDeltaEvent {
                sequence_number: 1,
                item_id: "item-1".into(),
                output_index: 0,
                content_index: 0,
                delta: "no".into(),
            });
        assert!(responses_event_may_have_output(&refusal));

        let backend_progress = rs::ResponseStreamEvent::ResponseWebSearchCallSearching(
            rs_types::ResponseWebSearchCallSearchingEvent {
                sequence_number: 2,
                output_index: 0,
                item_id: "search-1".into(),
            },
        );
        assert!(responses_event_may_have_output(&backend_progress));
    }

    #[tokio::test]
    async fn tracked_stream_marks_non_forwarded_refusal_as_output() {
        let output_observed = Arc::new(AtomicBool::new(false));
        let refusal =
            rs::ResponseStreamEvent::ResponseRefusalDelta(rs_types::ResponseRefusalDeltaEvent {
                sequence_number: 0,
                item_id: "item-1".into(),
                output_index: 0,
                content_index: 0,
                delta: "no".into(),
            });
        let raw = stream::iter(vec![Ok(refusal), Ok(completed_event())]).boxed();
        let _ = collect(stream_responses_tracked(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            Arc::clone(&output_observed),
            FailedResponseCapture::default(),
        ))
        .await;

        assert!(output_observed.load(Ordering::Relaxed));
    }

    /// A server-side code-interpreter run surfaces as a generic backend tool
    /// call (started on InProgress, completed on OutputItemDone) named
    /// "code_interpreter" — the same shape as x_search — so it is no longer
    /// silently dropped from the event stream.
    #[tokio::test]
    async fn code_interpreter_forwards_backend_tool_call() {
        let in_progress = rs::ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(
            rs_types::ResponseCodeInterpreterCallInProgressEvent {
                sequence_number: 0,
                output_index: 0,
                item_id: "ci-1".into(),
            },
        );
        let done = rs::ResponseStreamEvent::ResponseOutputItemDone(
            rs_types::ResponseOutputItemDoneEvent {
                sequence_number: 1,
                output_index: 0,
                item: rs_types::OutputItem::CodeInterpreterCall(
                    rs_types::CodeInterpreterToolCall {
                        code: Some("print(1)".into()),
                        container_id: "cont-1".into(),
                        id: "ci-1".into(),
                        outputs: None,
                        status: rs_types::CodeInterpreterToolCallStatus::Completed,
                    },
                ),
            },
        );
        let raw = stream::iter(vec![Ok(in_progress), Ok(done), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                SamplingEvent::BackendToolCallStarted { call_id, name, .. }
                    if call_id == "ci-1" && name == "code_interpreter"
            )),
            "expected a code_interpreter BackendToolCallStarted, got {events:?}"
        );
        let completed = events.iter().find_map(|e| match e {
            SamplingEvent::BackendToolCallCompleted {
                call_id,
                name,
                result,
                ..
            } if name == "code_interpreter" => Some((call_id.clone(), result.clone())),
            _ => None,
        });
        let (call_id, result) = completed.expect("a code_interpreter BackendToolCallCompleted");
        assert_eq!(call_id, "ci-1");
        let result = result.expect("serialized code-interpreter payload");
        assert_eq!(result["code"], "print(1)");
    }

    fn function_call_added_event(
        output_index: u32,
        call_id: &str,
        name: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemAdded(rs_types::ResponseOutputItemAddedEvent {
            sequence_number: 0,
            output_index,
            item: rs_types::OutputItem::FunctionCall(rs_types::FunctionToolCall {
                arguments: String::new(),
                call_id: call_id.into(),
                name: name.into(),
                id: None,
                status: None,
            }),
        })
    }

    fn function_call_args_delta_event(output_index: u32, delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(
            rs_types::ResponseFunctionCallArgumentsDeltaEvent {
                sequence_number: 0,
                item_id: format!("item-{output_index}"),
                output_index,
                delta: delta.into(),
            },
        )
    }

    type Delta = (u32, Option<String>, Option<String>, Option<String>);

    /// Extract all ToolCallDelta events as (tool_index, id, name, arguments_delta).
    fn tool_call_deltas(evs: &[SamplingEvent]) -> Vec<Delta> {
        evs.iter()
            .filter_map(|e| match e {
                SamplingEvent::ToolCallDelta {
                    tool_index,
                    id,
                    name,
                    arguments_delta,
                    ..
                } => Some((
                    *tool_index,
                    id.clone(),
                    name.clone(),
                    arguments_delta.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn function_call_emits_initial_id_name_then_arg_deltas() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_xyz", "do_thing")),
            Ok(function_call_args_delta_event(0, "{\"x\":")),
            Ok(function_call_args_delta_event(0, "1}")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].0, 0);
        assert_eq!(deltas[0].1.as_deref(), Some("call_xyz"));
        assert_eq!(deltas[0].2.as_deref(), Some("do_thing"));
        assert_eq!(deltas[0].3, None);
        assert_eq!(deltas[1].0, 0);
        assert_eq!(deltas[1].1, None);
        assert_eq!(deltas[1].2, None);
        assert_eq!(deltas[1].3.as_deref(), Some("{\"x\":"));
        assert_eq!(deltas[2].3.as_deref(), Some("1}"));
    }

    #[tokio::test]
    async fn function_call_args_delta_without_added_event_is_dropped() {
        // ArgumentsDelta with no preceding OutputItemAdded has no
        // output_index → tool_index mapping; drop silently.
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_args_delta_event(7, "{\"oops\":1}")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        assert_eq!(tool_call_deltas(&evs).len(), 0);
    }

    #[tokio::test]
    async fn multiple_function_calls_get_distinct_tool_indices() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_a", "tool_a")),
            Ok(function_call_added_event(1, "call_b", "tool_b")),
            Ok(function_call_args_delta_event(0, "a-args")),
            Ok(function_call_args_delta_event(1, "b-args")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        assert_eq!(deltas.len(), 4);
        assert_eq!(deltas[0].0, 0);
        assert_eq!(deltas[0].1.as_deref(), Some("call_a"));
        assert_eq!(deltas[1].0, 1);
        assert_eq!(deltas[1].1.as_deref(), Some("call_b"));
        assert_eq!(deltas[2].0, 0);
        assert_eq!(deltas[2].3.as_deref(), Some("a-args"));
        assert_eq!(deltas[3].0, 1);
        assert_eq!(deltas[3].3.as_deref(), Some("b-args"));
    }

    #[tokio::test]
    async fn doom_loop_collector_signals_land_on_completed_response() {
        use xai_grok_sampling_types::doom_loop::{
            DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA,
        };
        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb(DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA));
        let raw = stream::iter(vec![Ok(text_delta_event("hello")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 1);
                assert_eq!(
                    response.doom_loop_signals[0].raw,
                    "tail_repetition:4@response"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// An armed collector holding a confident signal aborts the attempt with
    /// a retryable doom-loop failure; disarmed, the same stream completes and
    /// the signals ride the response instead.
    #[tokio::test]
    async fn confident_signal_aborts_stream_unless_disarmed() {
        let confident = r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking"]}}"#;

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        let raw = stream::iter(vec![Ok(text_delta_event("hi")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(
                    error.kind,
                    crate::events::SamplingErrorKind::DoomLoopDetected
                );
                assert!(error.is_retryable);
                assert_eq!(
                    error.doom_loop_triggers.as_deref(),
                    Some(&["tail_repetition:8@thinking".to_string()][..])
                );
            }
            other => panic!("expected Failed(DoomLoopDetected), got {other:?}"),
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        collector.disarm_abort();
        let raw = stream::iter(vec![Ok(text_delta_event("hi")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 1);
            }
            other => panic!("expected Completed after disarm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn doom_loop_signals_empty_without_collector_or_triggers() {
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // A collector that never saw a trigger also leaves the field empty.
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(crate::doom_loop::DoomLoopSignalCollector::default()),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    // ── Durable-output recovery ─────────────────────────────────────────

    fn output_message_item(id: &str, text: &str) -> rs_types::OutputItem {
        rs_types::OutputItem::Message(rs_types::OutputMessage {
            content: vec![rs_types::OutputMessageContent::OutputText(
                rs_types::OutputTextContent {
                    annotations: vec![],
                    logprobs: None,
                    text: text.into(),
                },
            )],
            id: id.into(),
            role: rs_types::AssistantRole::Assistant,
            status: rs_types::OutputStatus::Completed,
        })
    }

    fn function_call_item(call_id: &str, name: &str, arguments: &str) -> rs_types::OutputItem {
        rs_types::OutputItem::FunctionCall(rs_types::FunctionToolCall {
            arguments: arguments.into(),
            call_id: call_id.into(),
            name: name.into(),
            id: Some(format!("fc_{call_id}")),
            status: Some(rs_types::OutputStatus::Completed),
        })
    }

    fn output_item_done_event(
        output_index: u32,
        item: rs_types::OutputItem,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemDone(rs_types::ResponseOutputItemDoneEvent {
            sequence_number: 0,
            output_index,
            item,
        })
    }

    /// Completed output items survive a stream that dies before its terminal
    /// event: the turn is rebuilt from them (deduplicated by output index)
    /// instead of being discarded.
    #[tokio::test]
    async fn durable_items_recover_when_terminal_event_is_missing() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(output_item_done_event(
                0,
                output_message_item("msg-1", "partial answer"),
            )),
            Ok(output_item_done_event(
                1,
                function_call_item("call_1", "do_thing", "{\"x\":1}"),
            )),
            // A re-emitted done frame for the same index must not duplicate.
            Ok(output_item_done_event(
                1,
                function_call_item("call_1", "do_thing", "{\"x\":1}"),
            )),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                let assistants: Vec<_> = response
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        ConversationItem::Assistant(a) => Some(a),
                        _ => None,
                    })
                    .collect();
                assert_eq!(assistants.len(), 1, "exactly one assistant item");
                assert_eq!(assistants[0].content.as_ref(), "partial answer");
                assert_eq!(assistants[0].tool_calls.len(), 1, "no duplicated call");
                assert_eq!(assistants[0].tool_calls[0].id.as_ref(), "call_1");
                assert_eq!(assistants[0].tool_calls[0].name, "do_thing");
                assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A recovery whose last item is the completed assistant message is a
    /// terminal completion (`Stop`): the message finished before the stream
    /// died, so it must reach the caller instead of being reclassified as a
    /// max-token truncation by the retry loop.
    #[tokio::test]
    async fn durable_message_only_recovery_completes_terminally() {
        let raw = stream::iter(vec![Ok(output_item_done_event(
            0,
            output_message_item("msg-1", "whole message"),
        ))])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.assistant_text(), "whole message");
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A recovery that never saw the message complete (reasoning item only)
    /// may be missing its tail: it stays Incomplete and surfaces as `Length`.
    #[tokio::test]
    async fn durable_recovery_without_completed_message_reports_length() {
        let raw = stream::iter(vec![Ok(output_item_done_event(
            0,
            reasoning_item("rs-1", &["thinking"], None),
        ))])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::Length));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A retryable transport error after a completed output item takes the
    /// same durable recovery as a clean EOF instead of failing the attempt.
    #[tokio::test]
    async fn durable_items_recover_after_retryable_stream_error() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(output_item_done_event(
                0,
                output_message_item("msg-1", "done before the wire died"),
            )),
            Err(SamplingError::EventStreamError(
                "connection reset by peer".to_owned(),
            )),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.assistant_text(), "done before the wire died");
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A non-retryable mid-stream failure (a known event with a malformed
    /// payload) stays a hard error even when durable items exist.
    #[tokio::test]
    async fn non_retryable_stream_error_still_fails_despite_durable_items() {
        let serde_err =
            serde_json::from_str::<rs::ResponseStreamEvent>("{\"type\":\"response.created\"}")
                .unwrap_err();
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(output_item_done_event(
                0,
                output_message_item("msg-1", "durable"),
            )),
            Err(SamplingError::Serialization(serde_err)),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            matches!(events.last(), Some(SamplingEvent::Failed { .. })),
            "malformed known event must stay a hard error, got {events:?}"
        );
    }

    /// An idle timeout after a completed output item recovers durably
    /// instead of failing the attempt.
    #[tokio::test(start_paused = true)]
    async fn durable_items_recover_after_idle_timeout() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![Ok(
            output_item_done_event(0, output_message_item("msg-1", "before the stall")),
        )];
        let raw = stream::iter(events).chain(stream::pending()).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_millis(50),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.assistant_text(), "before the stall");
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// An idle timeout with no completed output item still fails as before.
    #[tokio::test(start_paused = true)]
    async fn idle_timeout_without_durable_items_still_fails() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> =
            vec![Ok(text_delta_event("hi"))];
        let raw = stream::iter(events).chain(stream::pending()).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_millis(50),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::IdleTimeout);
            }
            other => panic!("expected Failed(IdleTimeout), got {other:?}"),
        }
    }

    /// The terminal response stays authoritative: durable copies of done
    /// items must not leak into or duplicate a normally completed turn.
    #[tokio::test]
    async fn terminal_response_stays_authoritative_over_durable_items() {
        let mut final_resp = empty_completed_response();
        final_resp.output = vec![output_message_item("msg-1", "final")];
        let completed =
            rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
                response: final_resp,
                sequence_number: 2,
            });
        let raw = stream::iter(vec![
            Ok(output_item_done_event(
                0,
                output_message_item("msg-1", "streamed"),
            )),
            Ok(output_item_done_event(
                1,
                function_call_item("call_1", "do_thing", "{}"),
            )),
            Ok(completed),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.assistant_text(), "final");
                assert!(
                    response.tool_calls().is_empty(),
                    "durable function call must not leak past the terminal output"
                );
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// The exact wire sequence a ChatGPT/Codex turn produces, captured from
    /// `POST https://chatgpt.com/backend-api/codex/responses` with
    /// `gpt-5.6-luna`: the assistant message rides
    /// `response.output_item.done`, and `response.completed` reports
    /// `"status": "completed"` with `"output": []`.
    ///
    /// Taking that empty projection as authoritative erased the reply, and
    /// the resulting "empty response" made the sampler resample the same
    /// turn until the user cancelled — the user-visible bug of a turn that
    /// answers, then answers again, forever.
    #[tokio::test]
    async fn codex_completed_with_empty_output_keeps_the_streamed_message() {
        let message = output_message_item("msg_0edf", "ok");
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(rs::ResponseStreamEvent::ResponseCreated(
                rs_types::ResponseCreatedEvent {
                    response: build_response(rs_types::Status::InProgress),
                    sequence_number: 0,
                },
            )),
            Ok(rs::ResponseStreamEvent::ResponseInProgress(
                rs_types::ResponseInProgressEvent {
                    response: build_response(rs_types::Status::InProgress),
                    sequence_number: 1,
                },
            )),
            Ok(rs::ResponseStreamEvent::ResponseOutputItemAdded(
                rs_types::ResponseOutputItemAddedEvent {
                    sequence_number: 2,
                    output_index: 0,
                    item: message.clone(),
                },
            )),
            Ok(text_delta_event("ok")),
            Ok(rs::ResponseStreamEvent::ResponseOutputTextDone(
                rs_types::ResponseTextDoneEvent {
                    sequence_number: 6,
                    item_id: "msg_0edf".into(),
                    output_index: 0,
                    content_index: 0,
                    text: "ok".into(),
                    logprobs: None,
                },
            )),
            Ok(output_item_done_event(0, message)),
            // `output: []` — the Codex terminal projection.
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(
                    response.assistant_text(),
                    "ok",
                    "the completed message must survive an empty terminal output"
                );
                assert!(
                    !response.is_empty(),
                    "a turn that produced a message is not an empty response"
                );
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        // Exactly one terminal event, so the reply is delivered once.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    SamplingEvent::Completed { .. } | SamplingEvent::Failed { .. }
                ))
                .count(),
            1
        );
    }

    /// Same dialect, agentic shape: reasoning and a function call also live
    /// only on the done frames, so an empty terminal output must not drop
    /// the tool call (which would strand the turn instead of running it).
    #[tokio::test]
    async fn codex_empty_terminal_output_keeps_reasoning_and_tool_calls() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(output_item_done_event(
                0,
                reasoning_item("rs_1", &["Checking"], Some("enc-1")),
            )),
            Ok(output_item_done_event(
                1,
                function_call_item("call_1", "do_thing", "{\"x\":1}"),
            )),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
                let calls = response.tool_calls();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "do_thing");
                assert_eq!(calls[0].arguments.as_ref(), "{\"x\":1}");
                let reasoning = reasoning_siblings(response);
                assert_eq!(reasoning.len(), 1);
                assert_eq!(reasoning[0].encrypted_content.as_deref(), Some("enc-1"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A terminal response that is empty *and* has no completed items stays
    /// an empty turn: the splice may not invent content.
    #[tokio::test]
    async fn empty_terminal_output_without_durable_items_stays_empty() {
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.is_empty());
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// No terminal event and no completed items still fails as before.
    #[tokio::test]
    async fn missing_terminal_without_durable_items_still_fails() {
        let raw = stream::iter(vec![Ok(text_delta_event("hi"))]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── Reasoning items, summary attribution, event policy ──────────────

    fn reasoning_item(
        id: &str,
        summaries: &[&str],
        encrypted: Option<&str>,
    ) -> rs_types::OutputItem {
        rs_types::OutputItem::Reasoning(rs_types::ReasoningItem {
            id: id.into(),
            summary: summaries
                .iter()
                .map(|text| {
                    rs_types::SummaryPart::SummaryText(rs_types::SummaryTextContent {
                        text: (*text).into(),
                    })
                })
                .collect(),
            content: None,
            encrypted_content: encrypted.map(str::to_owned),
            status: Some(rs_types::OutputStatus::Completed),
        })
    }

    fn summary_delta_event(
        output_index: u32,
        summary_index: u32,
        item_id: &str,
        delta: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseReasoningSummaryTextDelta(
            rs_types::ResponseReasoningSummaryTextDeltaEvent {
                sequence_number: 0,
                item_id: item_id.into(),
                output_index,
                summary_index,
                delta: delta.into(),
            },
        )
    }

    fn reasoning_siblings(response: &ConversationResponse) -> Vec<&rs_types::ReasoningItem> {
        response
            .items
            .iter()
            .filter_map(|item| match item {
                ConversationItem::Reasoning(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    fn summary_texts(reasoning: &rs_types::ReasoningItem) -> Vec<&str> {
        reasoning
            .summary
            .iter()
            .map(|part| match part {
                rs_types::SummaryPart::SummaryText(t) => t.text.as_str(),
            })
            .collect()
    }

    /// A reasoning item in the terminal response keeps its wire identity —
    /// item id and `encrypted_content` — in the conversation history, so a
    /// later turn can replay it to the provider verbatim.
    #[tokio::test]
    async fn terminal_reasoning_item_keeps_id_and_encrypted_content() {
        let mut final_resp = empty_completed_response();
        final_resp.output = vec![
            reasoning_item("rs_1", &["thought summary"], Some("enc-blob-1")),
            output_message_item("msg-1", "the answer"),
        ];
        let completed =
            rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
                response: final_resp,
                sequence_number: 1,
            });
        let raw = stream::iter(vec![Ok(completed)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                let reasoning = reasoning_siblings(response);
                assert_eq!(reasoning.len(), 1);
                assert_eq!(reasoning[0].id, "rs_1");
                assert_eq!(
                    reasoning[0].encrypted_content.as_deref(),
                    Some("enc-blob-1")
                );
                assert_eq!(summary_texts(reasoning[0]), vec!["thought summary"]);
                assert_eq!(response.assistant_text(), "the answer");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A reasoning item that completed via `response.output_item.done`
    /// survives a stream that dies before its terminal event, encrypted
    /// content included (encrypted-reasoning durable recovery).
    #[tokio::test]
    async fn durable_recovery_keeps_encrypted_reasoning_item() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(output_item_done_event(
                0,
                reasoning_item("rs_enc", &[], Some("opaque-encrypted-bytes")),
            )),
            Ok(output_item_done_event(
                1,
                output_message_item("msg-1", "partial"),
            )),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                let reasoning = reasoning_siblings(response);
                assert_eq!(reasoning.len(), 1);
                assert_eq!(reasoning[0].id, "rs_enc");
                assert_eq!(
                    reasoning[0].encrypted_content.as_deref(),
                    Some("opaque-encrypted-bytes")
                );
                assert_eq!(response.assistant_text(), "partial");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Mixed recovery: a turn holding reasoning + a function call + text is
    /// rebuilt whole from its durable items — the reasoning keeps its
    /// identity and the tool call keeps its complete arguments, so the
    /// replayed history is not missing the context the call is bound to.
    #[tokio::test]
    async fn mixed_reasoning_and_tool_durable_recovery_is_lossless() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(output_item_done_event(
                0,
                reasoning_item("rs_mix", &["will call the tool"], Some("enc-mix")),
            )),
            Ok(output_item_done_event(
                1,
                function_call_item("call_9", "run_query", "{\"sql\":\"select 1\"}"),
            )),
            Ok(output_item_done_event(
                2,
                output_message_item("msg-9", "running it"),
            )),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                let reasoning = reasoning_siblings(response);
                assert_eq!(reasoning.len(), 1);
                assert_eq!(reasoning[0].id, "rs_mix");
                assert_eq!(reasoning[0].encrypted_content.as_deref(), Some("enc-mix"));
                let assistant = response.assistant().expect("assistant item");
                assert_eq!(assistant.content.as_ref(), "running it");
                assert_eq!(assistant.tool_calls.len(), 1);
                assert_eq!(assistant.tool_calls[0].id.as_ref(), "call_9");
                assert_eq!(assistant.tool_calls[0].name, "run_query");
                assert_eq!(
                    assistant.tool_calls[0].arguments.as_ref(),
                    "{\"sql\":\"select 1\"}",
                    "tool arguments must replay complete from the durable item"
                );
                assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A client-executed custom tool call recovers its full input from the
    /// durable item even when no delta ever streamed it.
    #[tokio::test]
    async fn custom_tool_call_durable_recovery_keeps_input() {
        let done = output_item_done_event(
            0,
            rs_types::OutputItem::CustomToolCall(
                serde_json::from_value::<rs_types::CustomToolCall>(serde_json::json!({
                    "call_id": "call-7",
                    "id": "ctc-7",
                    "name": "grep_tool",
                    "input": "pattern: needle",
                }))
                .expect("custom tool call fields satisfy the schema"),
            ),
        );
        let raw = stream::iter(vec![Ok(done)]).boxed();
        let events = collect(stream_responses_with_client_custom_tools(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            vec!["grep_tool".to_string()],
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                let assistant = response.assistant().expect("assistant item");
                assert_eq!(assistant.tool_calls.len(), 1);
                let call = &assistant.tool_calls[0];
                assert!(call.is_custom(), "custom envelope id expected");
                assert_eq!(call.call_id(), "call-7");
                assert_eq!(call.custom_item_id(), Some("ctc-7"));
                assert_eq!(call.name, "grep_tool");
                assert_eq!(call.arguments.as_ref(), "pattern: needle");
                assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Two reasoning items with their own summaries stay two distinct
    /// history items: neither the durable path nor the item conversion may
    /// blend one item's summary into the other (SPEC §10.1).
    #[tokio::test]
    async fn two_reasoning_items_keep_summaries_separate_in_history() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            // Interleaved summary deltas for both items (UI channel).
            Ok(summary_delta_event(0, 0, "rs_a", "A summarizes. ")),
            Ok(summary_delta_event(1, 0, "rs_b", "B summarizes. ")),
            Ok(summary_delta_event(0, 1, "rs_a", "A concludes.")),
            Ok(summary_delta_event(1, 1, "rs_b", "B concludes.")),
            // Durable copies, then the stream dies before its terminal.
            Ok(output_item_done_event(
                0,
                reasoning_item("rs_a", &["A summarizes. ", "A concludes."], None),
            )),
            Ok(output_item_done_event(
                1,
                reasoning_item("rs_b", &["B summarizes. ", "B concludes."], None),
            )),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                let reasoning = reasoning_siblings(response);
                assert_eq!(reasoning.len(), 2, "one history item per reasoning item");
                assert_eq!(reasoning[0].id, "rs_a");
                assert_eq!(
                    summary_texts(reasoning[0]),
                    vec!["A summarizes. ", "A concludes."]
                );
                assert_eq!(reasoning[1].id, "rs_b");
                assert_eq!(
                    summary_texts(reasoning[1]),
                    vec!["B summarizes. ", "B concludes."]
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Distinct summary parts are separate paragraphs on the thought
    /// channel: a part boundary emits a "\n\n" break, and deltas within one
    /// part stay unseparated. Without the break, Codex's multi-part
    /// summaries fuse into one line ("**A****B**").
    #[tokio::test]
    async fn summary_part_boundaries_break_paragraphs_on_the_thought_channel() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(summary_delta_event(0, 0, "rs_a", "**Planning the task**")),
            Ok(summary_delta_event(0, 0, "rs_a", " continued")),
            Ok(summary_delta_event(0, 1, "rs_a", "**Verifying the env**")),
            Ok(summary_delta_event(1, 0, "rs_b", "**Next item**")),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let reasoning_texts: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                SamplingEvent::ChannelToken {
                    channel: SamplingChannel::Reasoning,
                    text,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning_texts.concat(),
            "**Planning the task** continued\n\n**Verifying the env**\n\n**Next item**"
        );
        assert_eq!(
            reasoning_texts
                .iter()
                .filter(|t| t.as_str() == "\n\n")
                .count(),
            2,
            "exactly one break per part boundary"
        );
    }

    /// Interleaved summary deltas from two reasoning items land in the
    /// recovery capture attributed by item id + summary index: the replay
    /// yields one item per id, each holding only its own text in summary
    /// order.
    #[tokio::test]
    async fn interleaved_summary_deltas_stay_attributed_in_capture() {
        let capture = FailedResponseCapture::armed();
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(summary_delta_event(0, 0, "rs_a", "A first. ")),
            Ok(summary_delta_event(1, 0, "rs_b", "B first. ")),
            Ok(summary_delta_event(0, 1, "rs_a", "A second.")),
            Ok(summary_delta_event(1, 1, "rs_b", "B second.")),
            Err(SamplingError::EventStreamError("conn reset".into())),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses_tracked(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            Arc::new(AtomicBool::new(false)),
            capture.clone(),
        ))
        .await;
        assert!(matches!(events.last(), Some(SamplingEvent::Failed { .. })));

        let items = capture.take_items();
        let reasoning: Vec<&rs_types::ReasoningItem> = items
            .iter()
            .filter_map(|item| match item {
                ConversationItem::Reasoning(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning.len(), 2, "one replayed item per reasoning item");
        let text_of = |r: &rs_types::ReasoningItem| {
            r.content
                .as_ref()
                .map(|parts| parts.iter().map(|p| p.text.as_str()).collect::<String>())
                .unwrap_or_default()
        };
        assert_eq!(reasoning[0].id, "rs_a");
        assert_eq!(text_of(reasoning[0]), "A first. A second.");
        assert_eq!(reasoning[1].id, "rs_b");
        assert_eq!(text_of(reasoning[1]), "B first. B second.");
    }

    /// After a retry, the terminal `output` list is authoritative for the
    /// replay capture too: items recorded from earlier durable frames are
    /// superseded, not duplicated.
    #[tokio::test]
    async fn terminal_output_supersedes_captured_durable_items() {
        let capture = FailedResponseCapture::armed();
        let mut final_resp = empty_completed_response();
        final_resp.output = vec![output_message_item("msg-1", "final text")];
        let completed =
            rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
                response: final_resp,
                sequence_number: 3,
            });
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(output_item_done_event(
                0,
                reasoning_item("rs_pre", &["stale thought"], None),
            )),
            Ok(output_item_done_event(
                1,
                output_message_item("msg-1", "streamed text"),
            )),
            Ok(completed),
        ];
        let raw = stream::iter(events).boxed();
        let _ = collect(stream_responses_tracked(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            Arc::new(AtomicBool::new(false)),
            capture.clone(),
        ))
        .await;

        let items = capture.take_items();
        assert_eq!(items.len(), 1, "terminal output replaces durable copies");
        match &items[0] {
            ConversationItem::Assistant(a) => assert_eq!(a.content.as_ref(), "final text"),
            other => panic!("expected the terminal assistant text, got {other:?}"),
        }
    }

    // ── Event policy: malformed vs unknown event kinds ──────────────────

    fn serialization_error_for(payload: &str) -> SamplingError {
        SamplingError::Serialization(
            serde_json::from_str::<rs::ResponseStreamEvent>(payload)
                .expect_err("payload must not parse as a known stream event"),
        )
    }

    #[test]
    fn unknown_event_kind_classifier() {
        // Unknown top-level event type → ignorable.
        assert!(is_unknown_response_event_kind(&serialization_error_for(
            r#"{"type":"response.reticulating_splines","sequence_number":1}"#
        )));
        // Known event type with a malformed payload → hard error.
        assert!(!is_unknown_response_event_kind(&serialization_error_for(
            r#"{"type":"response.output_text.delta","sequence_number":1}"#
        )));
        // Non-serialization errors are never reclassified.
        assert!(!is_unknown_response_event_kind(
            &SamplingError::EventStreamError("conn reset".into())
        ));
    }

    /// A future side-channel event the enum does not know is skipped with a
    /// debug log; the stream keeps flowing and completes normally.
    #[tokio::test]
    async fn unknown_future_event_is_ignored_and_stream_completes() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Err(serialization_error_for(
                r#"{"type":"response.future_side_channel","sequence_number":1,"payload":{"x":1}}"#,
            )),
            Ok(text_delta_event("hello")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Failed { .. })),
            "unknown event kinds must not fail the stream"
        );
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.assistant_text(), "");
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(events.iter().any(|e| matches!(
            e,
            SamplingEvent::ChannelToken { text, .. } if text == "hello"
        )));
    }

    /// A known event kind whose payload is malformed fails the attempt
    /// explicitly rather than being skipped.
    #[tokio::test]
    async fn malformed_known_event_fails_the_stream() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(text_delta_event("hi")),
            // Missing `item_id` / `output_index` / `delta` etc.
            Err(serialization_error_for(
                r#"{"type":"response.output_text.delta","sequence_number":2}"#,
            )),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Serialization);
            }
            other => panic!("expected Failed(Serialization), got {other:?}"),
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. })),
            "a malformed known event must not be silently skipped"
        );
    }
}
