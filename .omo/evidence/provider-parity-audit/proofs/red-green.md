# Red/green mutation proofs (C002)

Generated: 2026-11-08T22:44:25.022Z

Each row reverts one fix to its pre-fix text, requires the named regression
test to fail, restores the fix, and requires it to pass.

| item | status | test / detail |
|---|---|---|
| L1-01 | PROVEN | compat::claude::tests::anthropic_cache_usage_is_inclusive_json_and_stream |
| L1-02 | PROVEN | compat::claude::tests::anthropic_render_usage_excludes_known_caches |
| L1-03 | PROVEN | compat::claude::tests::claude_stop_controls_cross_map |
| L1-04 | PROVEN | compat::claude::tests::claude_honors_max_completion_tokens |
| L1-05 | PROVEN | compat::claude::tests::anthropic_url_images_survive_chat_translation |
| L1-06 | PROVEN | compat::claude::tests::anthropic_tool_results_precede_user_followup |
| L1-07 | PROVEN | compat::claude::tests::anthropic_thinking_controls_survive_codex_plan |
| L1-08 | PROVEN | compat::claude::tests::claude_replays_signed_assistant_reasoning_before_tools |
| L1-09 | PROVEN | compat::claude::openai_to_anthropic_tests::excess_client_breakpoints_are_capped_at_the_provider_limit |
| L1-10 | PROVEN | zcode_native_client_breakpoints_are_capped_before_upstream |
| L1-11 | PROVEN | generic_anthropic_adapter_caps_client_breakpoints |
| L2-01 | PROVEN | url::tests::vertex_location_selects_regional_host |
| L2-02 | PROVEN | compat::gemini::tests::gemini_finish_reason_safety_is_not_success |
| L2-03 | PROVEN | compat::gemini::tests::gemini_buffered_usage_includes_thoughts_and_cache |
| L2-04 | PROVEN | compat::gemini::tests::gemini_stop_sequences_are_forwarded |
| L2-05 | PROVEN | compat::gemini::tests::gemini_local_ref_schema_is_resolved |
| L2-06 | PROVEN | compat::gemini::tests::gemini_tool_name_codec_roundtrips |
| L2-07 | PROVEN | compat::gemini::tests::gemini_explicit_budget_beats_model_default |
| L2-08 | PROVEN | compat::gemini::tests::gemini_response_format_on_generic_and_antigravity |
| L2-09 | PROVEN | management::oauth::tests::antigravity_login_waits_for_project_before_persist |
| L2-10 | PROVEN | compat::gemini::tests::gemini_input_audio_is_rejected |
| L3-01 | PROVEN | relay::native_responses_tests::native_codex_responses_drops_envelope_fields_the_backend_rejects |
| L3-02 | PROVEN | compat::render::tests::codex_done_only_arguments_survive_chat_rendering |
| L3-03 | PROVEN | compat::render::tests::codex_incomplete_preserves_chat_reason_and_usage |
| L3-04 | PROVEN | compat::request::tests::codex_long_call_ids_keep_injective_pairing |
| L3-05 | PROVEN | compat::responses::tests::responses_claude_controls_survive_translation |
| L3-06 | PROVEN | compat::responses::tests::responses_tool_output_array_retains_image_order |
| L3-07 | PROVEN | compat::responses::tests::compact_requests_summary_and_rejects_tool_only_reply |
| L3-08 | PROVEN | compat::request::tests::codex_explicit_reasoning_summary_is_retained |
| L3-09 | PROVEN | responses_client_receives_chat_stream_as_responses_events |
| L4-01 | PROVEN | pre_expiry_refresh::tests::a_rejected_grant_keeps_the_working_access_token_routable |
| L4-02 | PROVEN | pre_expiry_refresh::tests::only_a_rejected_grant_benches_the_account |
| L4-03 | PROVEN | refresh_exec::tests::a_status_error_never_renders_the_upstream_body |
| L4-04 | PROVEN | management::creds::tests::codex_import_keys_on_account_id |
| L4-05 | PROVEN | management::oauth::tests::reauth_preserves_operator_disabled |
| L4-06 | PROVEN | management::oauth::tests::oauth_write_takes_composition_lock |
| L4-07 | PROVEN | account::codex_refresh_claim_tests::codex_abandoned_claim_reaches_an_actionable_state |
| L4-08 | PROVEN | management::scalars::tests::weighted_round_robin_is_refused_because_no_selector_implements_it |
| L5-01 | PROVEN | relay::native_responses_tests::raw_sse_terminal_frames_control_eof_and_preserve_bytes |
| L5-02 | PROVEN | compat::bounded_stream_tests::translate_stall_budget_accounts_only_upstream_wait |
| L5-03 | PROVEN | compat::bounded_stream_tests::responses_and_gemini_idle_tick_is_complete_comment |
| L5-04 | PROVEN | compat::bounded_stream_tests::anthropic_empty_text_preamble_remains_retryable |
| L5-05 | PROVEN | relay::usage_capture_tests::cooldown_retry_after_http_date_precedes_reset |
| L6-01 | PROVEN | compat::cursor::tests::cursor_image_only_encodes_selected_image |
| L6-02 | PROVEN | compat::cursor::tests::cursor_kv_set_then_get_preserves_bytes |
| L6-04 | PROVEN | compat::kiro::tests::kiro_prior_tool_result_survives_new_user |
| L6-05 | PROVEN | compat::kiro::tests::kiro_redacted_reasoning_roundtrips |
| L6-06 | PROVEN | compat::kiro::tests::kiro_metadata_usage_survives_stop_and_eof |
| L6-07 | PROVEN | compat::command_code::tests::command_code_oauth_uses_cli_wire |
| L6-08 | PROVEN | interactions::tests::devin_function_result_continuation_pairs_pending_call |
| L6-EXEC | PROVEN | compat::cursor::tests::exec_write_rejects_dotdot_escape |

Proven total: 51
