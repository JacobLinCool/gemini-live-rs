# Runtime & Harness Sequence

This document is the cross-cutting sequence view of the system: how a host
application, harness, runtime, and the Gemini Live session interact across
foreground turns, background tool continuation, and notification delivery.

Use this file to reason about layer boundaries, ordering, and failure windows.
It complements the code-adjacent docs in the runtime and harness crates; when
this diagram disagrees with the implementation, the implementation wins and
this file should be corrected immediately.

```mermaid
sequenceDiagram
    autonumber

    actor U as "User"
    participant H as "Host App (CLI / Discord)"
    participant SM as "Session Manager / Host Wake Logic"
    participant HC as "HarnessController"
    participant HB as "HarnessRuntimeBridge"
    participant HR as "HarnessToolRuntime"
    participant RG as "HarnessToolRegistry"
    participant HS as "Harness Store"
    participant NP as "PassiveNotificationPump"
    participant RT as "ManagedRuntime / LiveRuntime"
    participant S as "gemini-live Session"
    participant M as "Gemini Model"
    participant TP as "Host ToolExecutor"
    participant X as "Third-Party Tool / OS / Local Command / Remote API"
    participant EV as "Harness Event Source (generic / heartbeat / scheduled)"

    Note over HC,NP: Harness layer
    Note over HS: Durable data on disk<br/>tasks/<id>/task.json<br/>tasks/<id>/events.jsonl<br/>tasks/<id>/result.json<br/>notifications/*.json<br/>memory/<scope>/<key>.json

    U->>H: Launch app or continue conversation
    H->>HS: open harness root
    HS-->>H: load durable tasks, notifications, memory
    H->>NP: recover_orphaned_deliveries()
    NP->>HS: delivered notification -> queued (if previous process died before ack)

    Note over H,HC: Host provides ToolProvider / ToolExecutor<br/>Harness owns merge, routing, budget policy, durable background continuation
    H->>HC: build controller with host tools
    HC->>RG: merge harness built-ins + host tool provider
    RG-->>HC: registered tools + advertised setup.tools
    HC->>HS: interrupt_stale_running_tasks(current runtime instance)
    HS-->>HC: stale `Running` tasks become `Interrupted`

    H->>SM: ensure_hot()
    alt no active logical runtime session
        H->>HC: advertised_tools()
        HC-->>H: merged setup.tools
        alt resumable handle exists in process memory
            H->>RT: connect_resumed(setup + resume handle)
            alt resumed connect succeeds
                RT->>S: open official Live session
                S->>M: setup(model, tools, system instruction, resumption handle)
                M-->>S: setupComplete
                S-->>RT: ServerEvent::SetupComplete
                RT-->>H: RuntimeEvent::Lifecycle(Connected)
            else resumed connect fails
                SM->>SM: clear stale resume handle
                alt recent turns exist for rehydrate
                    H->>RT: connect fresh with initial_history_in_client_content = true
                    RT->>S: open official Live session
                    S->>M: setup(model, tools, system instruction)
                    M-->>S: setupComplete
                    S-->>RT: ServerEvent::SetupComplete
                    H->>RT: send rehydrated conversation history
                    RT->>S: client content history
                    S->>M: replay recent turns
                    RT-->>H: RuntimeEvent::Lifecycle(Connected)
                else no rehydrate content
                    H->>RT: connect fresh with desired setup
                    RT->>S: open official Live session
                    S->>M: setup(model, tools, system instruction)
                    M-->>S: setupComplete
                    S-->>RT: ServerEvent::SetupComplete
                    RT-->>H: RuntimeEvent::Lifecycle(Connected)
                end
            end
        else no resumable handle available
            alt recent turns exist for rehydrate
                H->>RT: connect fresh with initial_history_in_client_content = true
                RT->>S: open official Live session
                S->>M: setup(model, tools, system instruction)
                M-->>S: setupComplete
                S-->>RT: ServerEvent::SetupComplete
                H->>RT: send rehydrated conversation history
                RT->>S: client content history
                S->>M: replay recent turns
                RT-->>H: RuntimeEvent::Lifecycle(Connected)
            else no rehydrate content
                H->>RT: connect fresh with desired setup
                RT->>S: open official Live session
                S->>M: setup(model, tools, system instruction)
                M-->>S: setupComplete
                S-->>RT: ServerEvent::SetupComplete
                RT-->>H: RuntimeEvent::Lifecycle(Connected)
            end
        end
    else runtime already hot
        SM-->>H: existing runtime session is ready
    end

    U->>H: Send text or audio or message
    H->>SM: ensure_hot()
    SM-->>H: active session ready
    H->>RT: send_text or send_audio or send_video
    RT->>S: client message
    S->>M: user input turn

    alt model responds without tool use
        M-->>S: model text / model audio / turnComplete
        S-->>RT: typed ServerEvent stream
        RT-->>H: RuntimeEvent::Server(...)
        H-->>U: render text / audio / Discord reply
    else model requests tool call
        M-->>S: toolCall(function name, args, call id)
        S-->>RT: ServerEvent::ToolCall(...)
        RT-->>HB: RuntimeEvent::ToolCallRequested(call)
        HB->>HC: handle_runtime_event(tool request)
        HC->>HR: spawn_tool_call(call)
        HR->>RG: route(function_name)

        alt host-provided blocking tool
            RG-->>HR: host capability
            HR->>TP: execute(call)
            TP->>X: actual blocking work

            alt completes within inline budget
                X-->>TP: result
                TP-->>HR: FunctionResponse
                HR-->>HC: completion(FunctionResponse)
                HC-->>HB: completion
                HB->>RT: send_tool_response(FunctionResponse)
                RT->>S: ToolResponseMessage
                S->>M: functionResponse
                M-->>S: follow-up answer
                S-->>RT: ServerEvent::ModelText / TurnComplete
                RT-->>H: RuntimeEvent::Server(...)
                H-->>U: show tool-informed answer

            else exceeds inline budget and can_continue_async_after_timeout = true
                Note over HR,HS: This is harness-owned async continuation<br/>not Gemini protocol async function calling
                HR->>HS: start_task(task.json status = running,<br/>runtime_instance_id, pid, origin_call_id)
                HR->>HS: append Created / Started / Progress events to events.jsonl
                Note over HR,HS: No semantic dedup runs here.<br/>If the model issues the same logical tool call twice,<br/>the harness creates two distinct tasks keyed by two call ids.
                HR-->>HC: FunctionResponse(short natural-language instruction)
                HC-->>HB: completion
                HB->>RT: send_tool_response(timeout FunctionResponse)
                RT->>S: ToolResponseMessage
                S->>M: functionResponse("taking longer; continuing in background")
                M-->>S: continue current turn
                S-->>RT: ServerEvent::ModelText / TurnComplete
                RT-->>H: RuntimeEvent::Server(...)
                H-->>U: model says work continues in background

                Note over TP,X: Original blocking execution keeps running after the response was already returned

                alt background execution eventually succeeds
                    X-->>TP: result
                    TP-->>HR: terminal FunctionResponse
                    HR->>HS: update task.json status = succeeded
                    HR->>HS: write result.json
                    HR->>HS: append Succeeded event
                    HR->>HS: enqueue notification.json status = queued kind = taskSucceeded

                else background execution eventually fails
                    X-->>TP: error
                    TP-->>HR: terminal error
                    HR->>HS: update task.json status = failed
                    HR->>HS: append Failed event
                    HR->>HS: enqueue notification.json status = queued kind = taskFailed

                else model or runtime cancels the tool call
                    M-->>S: toolCallCancellation(ids)
                    S-->>RT: ServerEvent::ToolCallCancellation(...)
                    RT-->>HB: RuntimeEvent::ToolCallCancellationRequested(ids)
                    HB->>HC: handle_runtime_event(tool cancellation)
                    HC->>HR: cancel(call_id)
                    Note over HR,TP: ToolExecutor::cancel() is best-effort and may be unsupported.<br/>Harness still aborts its own in-process execution future either way.
                    HR->>TP: try cancel(call_id)
                    alt task record already exists
                        HR->>HS: update task.json status = cancelled
                        HR->>HS: append Cancelled event
                        HR->>HS: enqueue notification.json status = queued kind = taskCancelled
                    else tool was still inline-only / no task yet
                        Note over HR,HS: No durable task exists to mark cancelled.
                    end
                end

            else exceeds budget but tool is inline-only
                Note over HR,TP: Harness cannot detach this tool from the current turn
                X-->>TP: wait until final result
                TP-->>HR: FunctionResponse
                HR-->>HC: completion(FunctionResponse)
                HC-->>HB: completion
                HB->>RT: send_tool_response(FunctionResponse)
                RT->>S: ToolResponseMessage
                S->>M: functionResponse
                M-->>S: follow-up answer
                S-->>RT: ServerEvent::ModelText / TurnComplete
                RT-->>H: RuntimeEvent::Server(...)
                H-->>U: show final answer
            end
        end
    end

    opt non-tool durable event source uses the same queue
        EV->>HS: enqueue_notification(kind = generic)<br/>examples: heartbeat / scheduled task / other durable system event
    end

    loop whenever host is idle and it is safe to interrupt the model
        H->>NP: can_deliver gate is true
        NP->>HS: list queued notifications
        Note over NP,HS: Queue order is updated_at_ms descending,<br/>then id ascending. Only one notification may be in flight.

        alt queued notification exists
            HS-->>NP: newest queued notification
            NP-->>H: PassiveNotificationDelivery(prompt, notification)

            Note over H,SM: Notification delivery is a normal send_text turn.<br/>The host may wake a dormant runtime first; an already-hot session is not required.
            Note over H,RT: It is not a special no-tools mode, so the model may still request tools.<br/>The host gate only blocks another notification while a turn is in flight.
            alt host successfully injects the notification turn
                H->>SM: ensure_hot(PassiveNotification)
                SM-->>H: active session ready
                H->>RT: send_text(notification prompt)
                RT->>S: client text
                S->>M: "background work finished or failed, tell the user"
                H-->>NP: deliver() = Ok
                NP->>HS: mark notification delivered
                NP->>NP: set in-flight notification id

                Note over H,NP: If the process dies after send_text succeeds but before<br/>mark_notification_delivered(), the notification stays queued and may be re-delivered later.

                M-->>S: user-facing follow-up (and possibly more tool calls)
                S-->>RT: ServerEvent::ModelText / TurnComplete
                RT-->>H: RuntimeEvent::Server(...)
                H-->>U: explain completed background task or durable event

                H->>NP: acknowledge_in_flight_notification()
                NP->>HS: mark notification acknowledged
            else host cannot inject the notification turn
                H-->>NP: deliver() = Err
                NP-->>H: keep notification queued for a later attempt
            end

        else no queued notification
            NP-->>H: no-op
        end
    end

    alt notification delivery is interrupted before acknowledgement
        H->>NP: requeue_in_flight_notification()
        NP->>HS: delivered -> queued
    end

    alt host process restarts later
        U->>H: relaunch app or new process wakes
        H->>HS: reopen same harness root
        H->>NP: recover_orphaned_deliveries()
        NP->>HS: any stale delivered notification -> queued
        H->>SM: ensure_hot()
        H->>RT: reconnect and continue
    end
```

## Implementation Notes

- Cancellation is best-effort. `ToolExecutor::cancel()` may simply return `false`, and the harness fallback is to abort its own in-process execution future and, when a background task already exists, mark that durable task cancelled.
- Discord and the CLI now both use `SessionManager` for wake / resume / rehydrate / dormancy. The CLI starts dormant, wakes on user input, media activity, tool completions, or passive notifications, and returns to dormant after its idle gate clears.
- Passive notification delivery is signal-driven inside one host process: queue mutations wake the pump immediately, and hosts separately signal gate changes when a session becomes interruptible. Idle hosts do not rely on fixed notification polling intervals anymore.
- Notification ordering is newest-first, not FIFO. The pump calls `list_notifications(status = queued, limit = 1)`, and the store sorts by `updated_at_ms` descending then `id` ascending.
- Session resumption fallback lives in `SessionManager::ensure_hot()`, not inside `ManagedRuntime::connect_resumed()`. When there is no usable handle, or a resumed connect fails, the manager explicitly falls back to `connect()` or `connect_with_setup_override(...)` plus rehydration.
- There is currently no semantic dedup for repeated model tool calls. Two logically identical calls become two independent background tasks if they both time out.
- There is currently no host-side recursion guard that forbids tool use during a passive-notification turn. The only guard in place is notification-delivery reentrancy: one notification in flight at a time, and only while the host considers the session idle enough to interrupt.
- The `Harness Event Source` participant is an extension-point contract around `enqueue_notification(...)`. Task terminal events are implemented; heartbeat / scheduled generic producers are not a completed end-to-end subsystem in this repository yet.
