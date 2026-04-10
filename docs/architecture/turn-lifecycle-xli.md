# xli Turn Lifecycle

Visual walkthrough of the complete turn lifecycle in xli (Rust).

---

## Mermaid Sequence Diagram: Full Turn

```mermaid
sequenceDiagram
    participant TUI as TUI (Ratatui)
    participant Session as Session / codex.rs
    participant State as SessionState
    participant Prewarm as SessionStartupPrewarmHandle
    participant Turn as ActiveTurn / TurnState
    participant Prompt as build_prompt()
    participant MCS as ModelClientSession
    participant MC as ModelClient
    participant Wire as Provider Wire (SSE)
    participant Tools as Tool Execution
    participant Loop as LoopDetector
    participant CTX as ContextManager

    TUI->>Session: submit user input
    Note over Session,State: Phase 1 — Session initialization
    Session->>State: SessionState::new(session_configuration)
    State->>State: ContextManager::new() → history
    State->>State: LoopDetector::new() → loop_detector
    Session->>Prewarm: schedule_startup_prewarm(base_instructions)
    Prewarm-->>MC: model_client.new_session() → ModelClientSession
    Prewarm-->>Wire: prewarm_websocket(generate=false)
    Note over Prewarm: JoinHandle stored in startup_prewarm

    Note over Session,Turn: Phase 2 — Turn creation
    Session->>Turn: ActiveTurn::default() → tasks, turn_state
    Session->>Turn: TurnState { tool_calls, token_usage_at_turn_start, ... }
    Session->>Prompt: build_prompt(input, tools, turn_context, base_instructions)
    Prompt-->>Session: Prompt { input: Vec<ResponseItem>, tools, base_instructions }

    Note over Session,MCS: Phase 3 — Model client setup
    Session->>Prewarm: consume_startup_prewarm_for_regular_turn()
    alt Prewarm ready
        Prewarm-->>MCS: SessionStartupPrewarmResolution::Ready(ModelClientSession)
    else Prewarm unavailable / timed out
        Session->>MC: model_client.new_session() → fresh ModelClientSession
    end
    Note over MCS: turn_state: Arc<OnceLock<String>> (x-codex-turn-state)

    Note over MCS,Wire: Phase 4 — Provider dispatch
    MCS->>MCS: effective_wire_api(model_slug)
    alt WireApi::Responses (OpenAI)
        alt WebSocket enabled
            MCS->>Wire: stream_responses_websocket(prompt, warmup=false)
            Wire-->>MCS: WebsocketStreamOutcome::Stream(ResponseStream)
        else WebSocket disabled / fallback
            MCS->>Wire: stream_responses_api(prompt) → POST /responses SSE
        end
    else WireApi::Messages (Anthropic)
        MCS->>MCS: conversation_to_anthropic_messages(input, supports_image)
        MCS->>MCS: extract_developer_blocks(input) → system[]
        MCS->>Wire: stream_messages_api(request) → POST /v1/messages SSE
    end

    Note over Wire,MCS: Phase 5 — Response streaming
    Wire-->>MCS: SSE events (content_block_*, message_delta, response.*)
    MCS-->>Session: mpsc::channel → ResponseEvent stream
    Session->>Loop: loop_detector.record_content(text)
    alt Content loop detected
        Loop-->>Session: true → inject LOOP_BREAK_MESSAGE
    end

    Note over Session,Tools: Phase 6 — Tool execution
    Session->>Tools: FunctionCall / LocalShellCall → tool approval
    Tools->>Tools: ExecPolicyManager.check_approval()
    alt Linux
        Tools->>Tools: spawn_command_under_linux_sandbox() [Landlock]
    else macOS
        Tools->>Tools: seatbelt sandbox profile
    end
    Tools-->>Session: FunctionCallOutput / tool result
    Session->>Loop: loop_detector.record_tool_call(name, args)
    alt Tool loop detected
        Loop-->>Session: true → inject LOOP_BREAK_MESSAGE, loop_detector.reset()
    end

    Note over Session,CTX: Phase 7 — Post-turn
    Session->>CTX: history.record_items(response_items, truncation_policy)
    Session->>State: consecutive_null_completions check / reset

    Note over Session: Phase 8 — Loop back
    alt Tool results pending
        Session->>Prompt: build_prompt() with tool outputs appended
        Session->>MCS: MCS.stream() → next request (same turn, reuse WS)
    else nextSpeaker continuation
        Session->>Session: schedule next turn
    else Turn complete
        Session->>TUI: emit final ResponseEvent
    end
```

---

## Mermaid Flowchart: xli State Architecture

```mermaid
flowchart TD
    subgraph SS["SessionState (state/session.rs:22)"]
        SC[session_configuration: SessionConfiguration]
        HX[history: ContextManager]
        RL[latest_rate_limits: Option&lt;RateLimitSnapshot&gt;]
        SR[server_reasoning_included: bool]
        DE[dependency_env: HashMap&lt;String,String&gt;]
        MD[mcp_dependency_prompted: HashSet&lt;String&gt;]
        PT[previous_turn_settings: Option&lt;PreviousTurnSettings&gt;]
        SP[startup_prewarm: Option&lt;SessionStartupPrewarmHandle&gt;]
        AC[active_connector_selection: HashSet&lt;String&gt;]
        PS[pending_session_start_source]
        GP[granted_permissions: Option&lt;PermissionProfile&gt;]
        CN[consecutive_null_completions: u32]
        LD[loop_detector: LoopDetector]
        PL[plan_state: PlanState]
    end

    subgraph SV["SessionServices (state/service.rs:31)"]
        MM[mcp_connection_manager: Arc&lt;RwLock&lt;McpConnectionManager&gt;&gt;]
        UE[unified_exec_manager: UnifiedExecProcessManager]
        AE[analytics_events_client: AnalyticsEventsClient]
        HK[hooks: Hooks]
        RO[rollout: Mutex&lt;Option&lt;RolloutRecorder&gt;&gt;]
        EP[exec_policy: Arc&lt;ExecPolicyManager&gt;]
        AM[auth_manager: Arc&lt;AuthManager&gt;]
        MN[models_manager: Arc&lt;ModelsManager&gt;]
        TA[tool_approvals: Mutex&lt;ApprovalStore&gt;]
        SK[skills_manager: Arc&lt;SkillsManager&gt;]
        PM[plugins_manager: Arc&lt;PluginsManager&gt;]
        MG[mcp_manager: Arc&lt;McpManager&gt;]
        SW[skills_watcher: Arc&lt;SkillsWatcher&gt;]
        NA[network_approval: Arc&lt;NetworkApprovalService&gt;]
        DB[state_db: Option&lt;StateDbHandle&gt;]
        MC[model_client: ModelClient]
        CM[code_mode_service: CodeModeService]
    end

    subgraph AT["ActiveTurn (state/turn.rs:27)"]
        TK[tasks: IndexMap&lt;String, RunningTask&gt;]
        TS[turn_state: Arc&lt;Mutex&lt;TurnState&gt;&gt;]
    end

    subgraph TS2["TurnState (state/turn.rs:98)"]
        PA[pending_approvals: HashMap&lt;String, oneshot::Sender&lt;ReviewDecision&gt;&gt;]
        PI[pending_input: Vec&lt;ResponseInputItem&gt;]
        MB[mailbox_delivery_phase: MailboxDeliveryPhase]
        GR[granted_permissions: Option&lt;PermissionProfile&gt;]
        TC[tool_calls: u64]
        TU[token_usage_at_turn_start: TokenUsage]
    end

    subgraph RT["RunningTask (state/turn.rs:69)"]
        DN[done: Arc&lt;Notify&gt;]
        KD[kind: TaskKind]
        CT[cancellation_token: CancellationToken]
        HD[handle: Arc&lt;AbortOnDropHandle&gt;]
        TX[turn_context: Arc&lt;TurnContext&gt;]
    end

    AT --> TS2
    AT --> RT
```

---

## Mermaid Flowchart: Anthropic Wire Conversion (`messages_wire.rs`)

```mermaid
flowchart TD
    A["conversation_to_anthropic_messages(input, supports_image)\nmessages_wire.rs:89"] --> B

    B["clean_orphaned_tool_calls(input)\nmessages_wire.rs:19\nS-005: strip unpaired tool_use / tool_result"] --> C

    C["Iterate cleaned ResponseItem[]"] --> D

    D{Item type?}

    D -->|Message role=user/assistant| E["text block → {type:text, text}\nimage block → {type:image, source:{type:url}} (supports_image)\n  or text placeholder S-008 if !supports_image\nmessages_wire.rs:98-147"]

    D -->|Message role=system or developer| F["skip — system injected separately\nmessages_wire.rs:100-103"]

    D -->|FunctionCall| G["tool_use block\n{type, id:call_id, name, input:JSON}\nmessages_wire.rs:150-167\nrole: assistant"]

    D -->|FunctionCallOutput| H["tool_result block\n{type, tool_use_id:call_id, content}\nmessages_wire.rs:169-179\nrole: user"]

    D -->|CustomToolCall| I["tool_use block\n{type, id, name, input}\nmessages_wire.rs:181-198\nrole: assistant"]

    D -->|CustomToolCallOutput| J["tool_result block\nmessages_wire.rs:200-210\nrole: user"]

    D -->|LocalShellCall| K["tool_use name=shell\nSynthetic ID if call_id=None\nmessages_wire.rs:212-234\nrole: assistant"]

    D -->|Reasoning| L["thinking/redacted_thinking block\nPrefers raw_wire_block for byte-identical replay\nmessages_wire.rs:237-303"]

    D -->|ToolSearchCall| M["tool_use name=tool_search\nmessages_wire.rs:306-329"]

    D -->|ToolSearchOutput| N["tool_result block\nmessages_wire.rs:331-349"]

    E --> O
    F --> O
    G --> O
    H --> O
    I --> O
    J --> O
    K --> O
    L --> O
    M --> O
    N --> O

    O["append_to_role(messages, role, blocks)\nmerge consecutive same-role messages\nmessages_wire.rs (helper)"]

    O --> P["strip_thinking_from_non_latest_assistant_messages()\nmessages_wire.rs:433\nAnthropic: thinking blocks only allowed in latest assistant msg"]

    P --> Q{"Last message role=assistant?"}
    Q -->|yes| R["Append synthetic user sentinel\n[Awaiting tool result] or [Continue]\nS-014: Vertex AI prefill guard\nmessages_wire.rs:372-388"]
    Q -->|no| S["Return Vec&lt;Value&gt; messages"]
    R --> S
```

---

## Mermaid Flowchart: Loop Detection (`loop_detection.rs`)

```mermaid
flowchart TD
    A["LoopDetector::new()\nloop_detection.rs:50"] --> B["tool_call_history: VecDeque&lt;(String,u64)&gt;\ncap=64 (MAX_HISTORY)\nloop_detection.rs:32"]
    A --> C["content_hashes: VecDeque&lt;u64&gt;\ncap=64\nloop_detection.rs:37"]
    A --> D["tool_loop_threshold=5\nDEFAULT_TOOL_LOOP_THRESHOLD\nloop_detection.rs:14"]
    A --> E["content_loop_threshold=10\nDEFAULT_CONTENT_LOOP_THRESHOLD\nloop_detection.rs:17"]

    subgraph TC["Tool Call Path"]
        TC1["record_tool_call(name, args)\nloop_detection.rs:60"] --> TC2["hash = hash_string(args)\nDefaultHasher/SipHash\nloop_detection.rs:61"]
        TC2 --> TC3{"len >= 64?"}
        TC3 -->|yes| TC4["pop_front()"]
        TC3 -->|no| TC5["push_back((name, hash))"]
        TC4 --> TC5
        TC5 --> TC6["detect_tool_loop()\nloop_detection.rs:88\nlast 5 entries identical (name+hash)?"]
        TC6 -->|true| TC7["Return true → inject LOOP_BREAK_MESSAGE\nloop_detection.rs:24"]
        TC6 -->|false| TC8["Return false"]
    end

    subgraph CC["Content Path"]
        CC1["record_content(content)\nloop_detection.rs:71"] --> CC2["hash = hash_string(content)"]
        CC2 --> CC3{"len >= 64?"}
        CC3 -->|yes| CC4["pop_front()"]
        CC3 -->|no| CC5["push_back(hash)"]
        CC4 --> CC5
        CC5 --> CC6["detect_content_loop()\nloop_detection.rs:106\nlast 10 hashes identical?"]
        CC6 -->|true| CC7["Return true → inject LOOP_BREAK_MESSAGE"]
        CC6 -->|false| CC8["Return false"]
    end

    subgraph RR["Reset Path"]
        RR1["reset()\nloop_detection.rs:81"] --> RR2["tool_call_history.clear()"]
        RR1 --> RR3["content_hashes.clear()"]
    end

    TC7 --> RR1
    CC7 --> RR1
```

---

## Mermaid Flowchart: Sandbox Architecture

```mermaid
flowchart TD
    A["Tool call arrives: FunctionCall / LocalShellCall"]
    A --> B["ExecPolicyManager.check_approval()\nexec_policy.rs"]
    B --> C{Approval decision}
    C -->|Already approved| D["ApprovalStore lookup\ntools/sandboxing.rs:40\nMutex&lt;ApprovalStore&gt; in SessionServices"]
    C -->|Needs approval| E["with_cached_approval()\ntools/sandboxing.rs:70"]
    E --> F["Present to user via TUI\nAwait ReviewDecision"]
    F --> G{Decision}
    G -->|Approved| H["Record in ApprovalStore"]
    G -->|Denied| I["Return sandbox_denied error\nunified_exec/errors.rs:37"]
    H --> D
    D --> J{Platform?}

    J -->|Linux| K["spawn_command_under_linux_sandbox()\nlandlock.rs:25\nLandlock file-system access control"]
    J -->|macOS| L["seatbelt.rs\nSandbox profile evaluation"]
    J -->|Windows| M["windows_sandbox.rs\nJob object / read grants\nwindows_sandbox_read_grants.rs"]

    K --> N["UnifiedExecProcessManager\nunified_exec/mod.rs:125\nmanages running processes"]
    L --> N
    M --> N
    N --> O["stdout/stderr streaming\nstart_streaming_output()\nunified_exec/async_watcher.rs:40"]
    O --> P["NetworkApprovalService\ntools/network_approval.rs\nArc&lt;NetworkApprovalService&gt; in SessionServices"]
    P --> Q["tool output → FunctionCallOutput"]
    Q --> R["tool_output_masking.rs\nmask sensitive patterns before history"]
```

---

## Mermaid Flowchart: ModelClient Architecture

```mermaid
flowchart TD
    subgraph Session["Session-scoped"]
        MC["ModelClient (client.rs:205)\nArc&lt;ModelClientState&gt;\n- auth_manager\n- conversation_id (ThreadId)\n- provider: ModelProviderInfo\n- disable_websockets: AtomicBool\n- cached_websocket_session: StdMutex&lt;WebsocketSession&gt;"]
    end

    subgraph Turn["Turn-scoped"]
        MCS["ModelClientSession (client.rs:222)\n- client: ModelClient\n- websocket_session: WebsocketSession\n- turn_state: Arc&lt;OnceLock&lt;String&gt;&gt;\n  (x-codex-turn-state sticky routing)"]
    end

    subgraph Prewarm["Startup Prewarm"]
        SPH["SessionStartupPrewarmHandle\nsession_startup_prewarm.rs:22\n- task: JoinHandle&lt;CodexResult&lt;ModelClientSession&gt;&gt;\n- started_at: Instant\n- timeout: Duration"]
    end

    MC -->|"new_session() → client.rs:353"| MCS
    MC -->|"schedule_startup_prewarm()\nsession_startup_prewarm.rs:159"| SPH
    SPH -->|"prewarm_websocket(generate=false)\nclient.rs:1606"| WS["WebSocket connection\nApiWebSocketConnection"]

    MCS -->|"stream() → client.rs:1665"| Dispatch

    subgraph Dispatch["Wire dispatch (client.rs:1676)"]
        EWA["effective_wire_api(model_slug)\nclient.rs:1743\nauto-upgrades Messages→Responses\nfor non-Anthropic models"]
        EWA -->|"WireApi::Responses + WS enabled"| WSPath["stream_responses_websocket()\nclient.rs:1467\nWebSocket transport\nreuse connection across requests"]
        EWA -->|"WireApi::Responses + WS disabled/fallback"| HTTPPath["stream_responses_api()\nclient.rs:1206\nHTTP POST /responses SSE"]
        EWA -->|"WireApi::Messages"| MsgPath["stream_messages_api()\nclient.rs:1304\nHTTP POST /v1/messages SSE"]
    end

    WSPath -->|"failure → try_switch_fallback_transport()\nclient.rs:1767"| Fallback["force_http_fallback()\nclient.rs:400\ndisable_websockets=true (AtomicBool)\nSession-scoped: all subsequent turns use HTTP"]
    Fallback --> HTTPPath

    WSPath -->|success| SSE["SSE / WS event stream\n→ ResponseEvent via mpsc"]
    HTTPPath --> SSE
    MsgPath --> SSE
```

---

## Source Reference Table

| Diagram Element | File | Line Range |
|---|---|---|
| `SessionState` struct | `codex-rs/core/src/state/session.rs` | 22–46 |
| `SessionState::new()` | `codex-rs/core/src/state/session.rs` | 50–68 |
| `ContextManager` field | `codex-rs/core/src/state/session.rs` | 24 |
| `LoopDetector` field | `codex-rs/core/src/state/session.rs` | 43 |
| `startup_prewarm` field | `codex-rs/core/src/state/session.rs` | 34 |
| `consecutive_null_completions` field | `codex-rs/core/src/state/session.rs` | 41 |
| `SessionServices` struct | `codex-rs/core/src/state/service.rs` | 31–63 |
| `ModelClient` in SessionServices | `codex-rs/core/src/state/service.rs` | 60 |
| `UnifiedExecProcessManager` in SessionServices | `codex-rs/core/src/state/service.rs` | 34 |
| `ExecPolicyManager` in SessionServices | `codex-rs/core/src/state/service.rs` | 45 |
| `NetworkApprovalService` in SessionServices | `codex-rs/core/src/state/service.rs` | 57 |
| `ApprovalStore` in SessionServices | `codex-rs/core/src/state/service.rs` | 49 |
| `ActiveTurn` struct | `codex-rs/core/src/state/turn.rs` | 27–30 |
| `TurnState` struct | `codex-rs/core/src/state/turn.rs` | 98–109 |
| `RunningTask` struct | `codex-rs/core/src/state/turn.rs` | 69–78 |
| `TaskKind` enum | `codex-rs/core/src/state/turn.rs` | 62–67 |
| `MailboxDeliveryPhase` enum | `codex-rs/core/src/state/turn.rs` | 43–51 |
| `ModelClient` struct | `codex-rs/core/src/client.rs` | 205–207 |
| `ModelClientSession` struct | `codex-rs/core/src/client.rs` | 222–236 |
| `ModelClientSession::stream()` | `codex-rs/core/src/client.rs` | 1665–1729 |
| `effective_wire_api()` | `codex-rs/core/src/client.rs` | 1743–1758 |
| `stream_responses_api()` | `codex-rs/core/src/client.rs` | 1206–1288 |
| `stream_messages_api()` | `codex-rs/core/src/client.rs` | 1304–1450 |
| `stream_responses_websocket()` | `codex-rs/core/src/client.rs` | 1467–end |
| `prewarm_websocket()` | `codex-rs/core/src/client.rs` | 1606–1656 |
| `force_http_fallback()` | `codex-rs/core/src/client.rs` | 400–418 |
| `try_switch_fallback_transport()` | `codex-rs/core/src/client.rs` | 1767–1778 |
| `X_CODEX_TURN_STATE_HEADER` | `codex-rs/core/src/client.rs` | 133 |
| `conversation_to_anthropic_messages()` | `codex-rs/core/src/messages_wire.rs` | 89–391 |
| `clean_orphaned_tool_calls()` | `codex-rs/core/src/messages_wire.rs` | 19–84 |
| `extract_developer_blocks()` | `codex-rs/core/src/messages_wire.rs` | 404–423 |
| `tools_to_anthropic_format()` | `codex-rs/core/src/messages_wire.rs` | 478–end |
| S-014 Vertex AI guard | `codex-rs/core/src/messages_wire.rs` | 359–388 |
| S-008 modality gating | `codex-rs/core/src/messages_wire.rs` | 116–138 |
| `LoopDetector` struct | `codex-rs/core/src/loop_detection.rs` | 30–40 |
| `LoopDetector::new()` | `codex-rs/core/src/loop_detection.rs` | 50–57 |
| `record_tool_call()` | `codex-rs/core/src/loop_detection.rs` | 60–67 |
| `record_content()` | `codex-rs/core/src/loop_detection.rs` | 71–78 |
| `detect_tool_loop()` | `codex-rs/core/src/loop_detection.rs` | 88–102 |
| `detect_content_loop()` | `codex-rs/core/src/loop_detection.rs` | 106–120 |
| `reset()` | `codex-rs/core/src/loop_detection.rs` | 81–84 |
| `hash_string()` | `codex-rs/core/src/loop_detection.rs` | 125–129 |
| `LOOP_BREAK_MESSAGE` | `codex-rs/core/src/loop_detection.rs` | 24–26 |
| `MAX_HISTORY` (64) | `codex-rs/core/src/loop_detection.rs` | 21 |
| `DEFAULT_TOOL_LOOP_THRESHOLD` (5) | `codex-rs/core/src/loop_detection.rs` | 14 |
| `DEFAULT_CONTENT_LOOP_THRESHOLD` (10) | `codex-rs/core/src/loop_detection.rs` | 17 |
| `SessionStartupPrewarmHandle` | `codex-rs/core/src/session_startup_prewarm.rs` | 22–26 |
| `SessionStartupPrewarmResolution` | `codex-rs/core/src/session_startup_prewarm.rs` | 28–35 |
| `Session::schedule_startup_prewarm()` | `codex-rs/core/src/session_startup_prewarm.rs` | 159–181 |
| `consume_startup_prewarm_for_regular_turn()` | `codex-rs/core/src/session_startup_prewarm.rs` | 183–197 |
| `spawn_command_under_linux_sandbox()` | `codex-rs/core/src/landlock.rs` | 25 |
| `UnifiedExecProcessManager` struct | `codex-rs/core/src/unified_exec/mod.rs` | 125 |
| `ApprovalStore` struct | `codex-rs/core/src/tools/sandboxing.rs` | 40 |
| `with_cached_approval()` | `codex-rs/core/src/tools/sandboxing.rs` | 70 |
| `build_prompt()` | `codex-rs/core/src/prompt_debug.rs` | (codex.rs entry) |
