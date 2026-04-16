use super::*;
use synaps_cli::{AttachEvent, AttachInbound, SyncState};

/// Handle IPC command from CLI
pub(crate) async fn handle_ipc_command(
    command: WatcherCommand,
    agents: Arc<Mutex<HashMap<String, ManagedAgent>>>,
) -> WatcherResponse {
    match command {
        WatcherCommand::Deploy { name } => {
            // Validate agent name
            if let Err(e) = validate_agent_name(&name) {
                return WatcherResponse::Error { message: e };
            }

            let mut agents = agents.lock().await;
            
            // Check if agent config exists
            let config_path = watcher_dir().join(&name).join("config.toml");
            if !config_path.exists() {
                return WatcherResponse::Error {
                    message: format!("Agent '{}' not found. Run: watcher init {}", name, name)
                };
            }

            // Load config
            let config = match AgentConfig::load(&config_path) {
                Ok(config) => config,
                Err(e) => return WatcherResponse::Error {
                    message: format!("Failed to load agent '{}': {}", name, e)
                }
            };

            // Check if already exists in map
            if let Some(agent) = agents.get_mut(&name) {
                if agent.is_running() {
                    return WatcherResponse::Error {
                        message: format!("Agent '{}' is already running", name)
                    };
                }
                // Un-stop it and restart if needed
                agent.stopped = false;
                if agent.config.agent.trigger == "always" {
                    match spawn_agent_auto(agent, "deploy restart").await {
                        Ok(()) => WatcherResponse::Ok {
                            message: format!("Agent '{}' deployed and started", name)
                        },
                        Err(e) => WatcherResponse::Error {
                            message: format!("Failed to start agent '{}': {}", name, e)
                        }
                    }
                } else {
                    WatcherResponse::Ok {
                        message: format!("Agent '{}' deployed", name)
                    }
                }
            } else {
                // Add new agent
                let mut agent = ManagedAgent::new(name.clone(), config_path, config);
                
                if agent.config.agent.trigger == "always" {
                    match spawn_agent(&mut agent, "deploy start").await {
                        Ok(()) => {
                            agents.insert(name.clone(), agent);
                            WatcherResponse::Ok {
                                message: format!("Agent '{}' deployed and started", name)
                            }
                        },
                        Err(e) => WatcherResponse::Error {
                            message: format!("Failed to start agent '{}': {}", name, e)
                        }
                    }
                } else {
                    agents.insert(name.clone(), agent);
                    WatcherResponse::Ok {
                        message: format!("Agent '{}' deployed", name)
                    }
                }
            }
        }

        WatcherCommand::Stop { name } => {
            let mut agents = agents.lock().await;
            if let Some(agent) = agents.get_mut(&name) {
                agent.stopped = true;
                if let Some(ref mut child) = agent.child {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                WatcherResponse::Ok {
                    message: format!("Agent '{}' stopped", name)
                }
            } else {
                WatcherResponse::Error {
                    message: format!("Agent '{}' not found or not running", name)
                }
            }
        }

        WatcherCommand::Status => {
            let agents = agents.lock().await;
            let agent_info: Vec<AgentStatusInfo> = agents.values()
                .map(|agent| agent.to_status_info())
                .collect();
            WatcherResponse::Status { agents: agent_info }
        }

        WatcherCommand::AgentStatus { name } => {
            let agents = agents.lock().await;
            if let Some(agent) = agents.get(&name) {
                WatcherResponse::AgentDetail {
                    info: agent.to_status_info()
                }
            } else {
                WatcherResponse::Error {
                    message: format!("Agent '{}' not found", name)
                }
            }
        }

        WatcherCommand::Attach { name, mode: _ } => {
            // Attach is handled specially in handle_ipc_connection — this shouldn't be reached
            // But just in case, return an error
            WatcherResponse::Error {
                message: format!("Attach for '{}' must be handled at connection level", name)
            }
        }
    }
}

/// IPC listener task
pub(crate) async fn ipc_listener(agents: Arc<Mutex<HashMap<String, ManagedAgent>>>) {
    let socket_path = watcher_dir().join("watcher.sock");
    
    // Check if socket exists and test if it's alive
    if socket_path.exists() {
        // Try to connect to existing socket
        if tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(&socket_path)).await.is_ok() {
            log("Another supervisor is already running");
            std::process::exit(1);
        } else {
            // Stale socket - remove it
            log("Removing stale socket");
            let _ = std::fs::remove_file(&socket_path);
        }
    }
    
    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(e) => {
            log(&format!("Failed to bind IPC socket: {}", e));
            return;
        }
    };

    // Set socket permissions to owner-only
    if let Err(e) = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)) {
        log(&format!("Failed to set socket permissions: {}", e));
        return;
    }

    log(&format!("IPC listening on {}", socket_path.display()));

    let semaphore = Arc::new(Semaphore::new(10)); // Max 10 concurrent IPC connections

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let agents = agents.clone();
                let permit = semaphore.clone().try_acquire_owned();
                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _ = handle_ipc_connection(stream, agents).await;
                            drop(permit); // Release on completion
                        });
                    }
                    Err(_) => {
                        // Too many connections — drop this one
                        log("IPC: too many concurrent connections, dropping");
                    }
                }
            }
            Err(e) => {
                log(&format!("IPC accept error: {}", e));
                break;
            }
        }
    }
}

/// Handle a single IPC connection
pub(crate) async fn handle_ipc_connection(
    stream: UnixStream,
    agents: Arc<Mutex<HashMap<String, ManagedAgent>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    
    buf_reader.read_line(&mut line).await?;
    let command: WatcherCommand = serde_json::from_str(line.trim())?;
    
    // Attach is special — it upgrades the connection to streaming mode
    if let WatcherCommand::Attach { ref name, ref mode } = command {
        let (bus_handle, sync_json) = {
            let agents_map = agents.lock().await;
            match agents_map.get(name) {
                Some(agent) if agent.bus_handle.is_some() => {
                    let bh = agent.bus_handle.clone().unwrap();
                    // Build a minimal sync state
                    let sync = SyncState {
                        agent_name: Some(name.clone()),
                        model: agent.config.agent.model.clone(),
                        thinking_level: agent.config.agent.thinking.clone(),
                        session_id: format!("session-{}", agent.session_count),
                        is_streaming: agent.is_running(),
                        turn_count: 0,
                        total_input_tokens: 0,
                        total_output_tokens: 0,
                        total_cost_usd: 0.0,
                        partial_text: None,
                        partial_thinking: None,
                        active_tool: None,
                        recent_events: Vec::new(),
                    };
                    let sync_str = serde_json::to_string(&sync).unwrap_or_default();
                    (bh, sync_str)
                }
                Some(_) => {
                    let resp = WatcherResponse::Error {
                        message: format!("Agent '{}' is not running in-process", name)
                    };
                    let resp_json = serde_json::to_string(&resp)?;
                    writer.write_all(resp_json.as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await?;
                    return Ok(());
                }
                None => {
                    let resp = WatcherResponse::Error {
                        message: format!("Agent '{}' not found", name)
                    };
                    let resp_json = serde_json::to_string(&resp)?;
                    writer.write_all(resp_json.as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await?;
                    return Ok(());
                }
            }
        };

        // Send AttachOk
        let resp = WatcherResponse::AttachOk { sync_state: sync_json };
        let resp_json = serde_json::to_string(&resp)?;
        writer.write_all(resp_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        // Switch to streaming mode
        handle_attach(buf_reader, writer, bus_handle, mode.clone()).await;
        return Ok(());
    }
    
    let response = handle_ipc_command(command, agents).await;
    let response_json = serde_json::to_string(&response)?;
    
    writer.write_all(response_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    
    Ok(())
}

/// Handle an attached streaming session
async fn handle_attach<R, W>(
    mut buf_reader: BufReader<R>,
    mut writer: W,
    bus_handle: synaps_cli::BusHandle,
    mode: String,
)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut event_rx = bus_handle.subscribe();
    let inbound_tx = bus_handle.inbound();
    let read_only = mode == "ro";

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Ok(e) => {
                        // MessageHistory is a TUI-local sync signal — it carries the full
                        // conversation Vec<Value> and is broadcast on every auto-save.
                        // Forwarding it to attached clients over NDJSON would send the
                        // entire message history down the wire every turn. Skip.
                        if matches!(e, synaps_cli::transport::AgentEvent::MessageHistory(_)) {
                            continue;
                        }
                        let wire = AttachEvent::Event { event: e };
                        let Ok(json) = serde_json::to_string(&wire) else { break };
                        if writer.write_all(json.as_bytes()).await.is_err() { break; }
                        if writer.write_all(b"\n").await.is_err() { break; }
                        let _ = writer.flush().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }

            line = async {
                let mut line = String::new();
                buf_reader.read_line(&mut line).await.map(|_| line)
            }, if !read_only => {
                match line {
                    Ok(ref l) if l.is_empty() => break, // EOF
                    Ok(ref l) => {
                        if let Ok(msg) = serde_json::from_str::<AttachInbound>(l) {
                            match msg {
                                AttachInbound::Message { content } => {
                                    let _ = inbound_tx.send(synaps_cli::transport::Inbound::Message { content });
                                }
                                AttachInbound::Cancel => {
                                    let _ = inbound_tx.send(synaps_cli::transport::Inbound::Cancel);
                                }
                                AttachInbound::Detach => break,
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

/// Send command to supervisor via IPC
pub(crate) async fn send_ipc_command(command: WatcherCommand) -> Result<WatcherResponse, String> {
    let socket_path = watcher_dir().join("watcher.sock");
    if !socket_path.exists() {
        return Err("Supervisor not running. Start with: watcher run".to_string());
    }
    
    // Add timeout to avoid hanging on stale socket
    let connect_result = tokio::time::timeout(
        Duration::from_secs(5),
        UnixStream::connect(&socket_path)
    ).await;
    
    let mut stream = match connect_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => {
            // Socket exists but can't connect — stale
            return Err("Supervisor socket is stale. Remove it and restart: watcher run".to_string());
        }
        Err(_) => {
            return Err("Supervisor not responding (timeout). Try: watcher run".to_string());
        }
    };
    
    let command_json = serde_json::to_string(&command)
        .map_err(|e| format!("Failed to serialize command: {}", e))?;
    
    stream.write_all(command_json.as_bytes()).await
        .map_err(|e| format!("Failed to send command: {}", e))?;
    stream.write_all(b"\n").await
        .map_err(|e| format!("Failed to send command: {}", e))?;
    stream.flush().await
        .map_err(|e| format!("Failed to send command: {}", e))?;
    
    let mut reader = BufReader::new(&mut stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await
        .map_err(|e| format!("Failed to read response: {}", e))?;
    
    serde_json::from_str(response_line.trim())
        .map_err(|e| format!("Failed to parse response: {}", e))
}
// ---------------------------------------------------------------------------
// Integration tests for the attach protocol (Phase 7d)
//
// These tests exercise `handle_attach` end-to-end using `tokio::io::duplex`
// as a stand-in for the Unix socket split. They validate the wire contract
// between an attached client and the in-process agent bus — events flow out,
// commands flow in, Detach cleans up, read-only mode suppresses inbound,
// and multiple attached clients see the same event stream.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod attach_tests {
    use super::*;
    use synaps_cli::{AgentEvent, AttachEvent, AttachInbound};
    use synaps_cli::transport::{AgentBus, Inbound};
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
    use tokio::time::{timeout, Duration};

    /// Wire an in-memory duplex pair into handle_attach, return the client side
    /// and a JoinHandle for the handler task.
    ///
    /// Returns: (client_reader, client_writer, handler_join, bus)
    fn spawn_attach_session(
        mode: &str,
    ) -> (
        TokioBufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<()>,
        AgentBus,
    ) {
        let bus = AgentBus::new();
        let handle = bus.handle();

        // client <-> server duplex pair
        let (server_r, client_w) = duplex(4096);
        let (client_r, server_w) = duplex(4096);

        let server_reader = TokioBufReader::new(server_r);
        let mode = mode.to_string();
        let join = tokio::spawn(async move {
            handle_attach(server_reader, server_w, handle, mode).await;
        });

        (TokioBufReader::new(client_r), client_w, join, bus)
    }

    /// Helper: read one NDJSON line from the client reader with a timeout,
    /// parse it as AttachEvent, panic on timeout/parse error.
    async fn recv_attach_event(
        client_reader: &mut TokioBufReader<tokio::io::DuplexStream>,
    ) -> AttachEvent {
        let mut line = String::new();
        let n = timeout(Duration::from_secs(2), client_reader.read_line(&mut line))
            .await
            .expect("client read timed out")
            .expect("client read error");
        assert!(n > 0, "client reader returned EOF");
        serde_json::from_str(line.trim()).expect("failed to parse AttachEvent")
    }

    /// Helper: send an AttachInbound as NDJSON to the client writer.
    async fn send_inbound(
        client_writer: &mut tokio::io::DuplexStream,
        inbound: &AttachInbound,
    ) {
        let json = serde_json::to_string(inbound).unwrap();
        client_writer.write_all(json.as_bytes()).await.unwrap();
        client_writer.write_all(b"\n").await.unwrap();
        client_writer.flush().await.unwrap();
    }

    // -------------------------------------------------------------------
    // Test 1: Bus event flows out to the attached client as AttachEvent::Event
    // -------------------------------------------------------------------
    #[tokio::test]
    async fn test_event_flows_out_to_client() {
        let (mut client_r, _client_w, join, bus) = spawn_attach_session("rw");

        // Give the handler time to subscribe to the bus
        tokio::time::sleep(Duration::from_millis(20)).await;

        bus.broadcast(AgentEvent::Text("hello from bus".to_string()));

        let ev = recv_attach_event(&mut client_r).await;
        match ev {
            AttachEvent::Event { event: AgentEvent::Text(t) } => {
                assert_eq!(t, "hello from bus");
            }
            other => panic!("expected Event(Text), got {:?}", other),
        }

        join.abort();
    }

    // -------------------------------------------------------------------
    // Test 2: Client Message flows through to the bus inbound channel
    // -------------------------------------------------------------------
    #[tokio::test]
    async fn test_client_message_reaches_bus_inbound() {
        let (_client_r, mut client_w, join, mut bus) = spawn_attach_session("rw");
        let mut inbound_rx = bus.take_inbound_rx().unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;

        send_inbound(&mut client_w, &AttachInbound::Message { content: "ping".into() }).await;

        let received = timeout(Duration::from_secs(2), inbound_rx.recv())
            .await
            .expect("inbound_rx timed out")
            .expect("inbound_rx closed");
        match received {
            Inbound::Message { content } => assert_eq!(content, "ping"),
            other => panic!("expected Inbound::Message, got {:?}", other),
        }

        join.abort();
    }

    // -------------------------------------------------------------------
    // Test 3: Client Cancel reaches bus inbound as Inbound::Cancel
    // -------------------------------------------------------------------
    #[tokio::test]
    async fn test_client_cancel_reaches_bus_inbound() {
        let (_client_r, mut client_w, join, mut bus) = spawn_attach_session("rw");
        let mut inbound_rx = bus.take_inbound_rx().unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;

        send_inbound(&mut client_w, &AttachInbound::Cancel).await;

        let received = timeout(Duration::from_secs(2), inbound_rx.recv())
            .await
            .expect("inbound_rx timed out")
            .expect("inbound_rx closed");
        assert!(matches!(received, Inbound::Cancel), "expected Inbound::Cancel, got {:?}", received);

        join.abort();
    }

    // -------------------------------------------------------------------
    // Test 4: Client Detach causes handle_attach to return cleanly
    // -------------------------------------------------------------------
    #[tokio::test]
    async fn test_detach_ends_session() {
        let (_client_r, mut client_w, join, _bus) = spawn_attach_session("rw");

        tokio::time::sleep(Duration::from_millis(20)).await;

        send_inbound(&mut client_w, &AttachInbound::Detach).await;

        // Give the handler a moment to notice and return
        let result = timeout(Duration::from_secs(2), join).await;
        assert!(result.is_ok(), "handle_attach did not return within 2s of Detach");
        // And it should have returned normally (not panicked)
        assert!(result.unwrap().is_ok(), "handle_attach task panicked");
    }

    // -------------------------------------------------------------------
    // Test 5: Read-only mode suppresses inbound traffic
    // In ro mode, client-side writes should NOT reach the bus inbound,
    // but events should still flow out to the client.
    // -------------------------------------------------------------------
    #[tokio::test]
    async fn test_readonly_suppresses_inbound() {
        let (mut client_r, mut client_w, join, mut bus) = spawn_attach_session("ro");
        let mut inbound_rx = bus.take_inbound_rx().unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;

        // Events still flow out
        bus.broadcast(AgentEvent::Text("visible".to_string()));
        let ev = recv_attach_event(&mut client_r).await;
        assert!(matches!(ev, AttachEvent::Event { .. }));

        // But client's inbound is ignored — even if the bytes arrive, no Inbound is produced
        send_inbound(&mut client_w, &AttachInbound::Message { content: "should be dropped".into() }).await;

        let res = timeout(Duration::from_millis(300), inbound_rx.recv()).await;
        assert!(res.is_err(), "ro mode must not forward inbound messages, got {:?}", res);

        join.abort();
    }

    // -------------------------------------------------------------------
    // Test 6: Two attached clients both see the same bus event
    // -------------------------------------------------------------------
    #[tokio::test]
    async fn test_multi_attach_fanout() {
        let bus = AgentBus::new();
        let h1 = bus.handle();
        let h2 = bus.handle();

        let (s1_r, c1_w) = duplex(4096);
        let (c1_r, s1_w) = duplex(4096);
        let (s2_r, c2_w) = duplex(4096);
        let (c2_r, s2_w) = duplex(4096);

        let j1 = tokio::spawn(async move {
            handle_attach(TokioBufReader::new(s1_r), s1_w, h1, "rw".to_string()).await;
        });
        let j2 = tokio::spawn(async move {
            handle_attach(TokioBufReader::new(s2_r), s2_w, h2, "rw".to_string()).await;
        });

        // Unused writers — keep alive
        let _ = c1_w;
        let _ = c2_w;

        tokio::time::sleep(Duration::from_millis(30)).await;

        bus.broadcast(AgentEvent::Text("shared".to_string()));

        let mut r1 = TokioBufReader::new(c1_r);
        let mut r2 = TokioBufReader::new(c2_r);
        let ev1 = recv_attach_event(&mut r1).await;
        let ev2 = recv_attach_event(&mut r2).await;

        for ev in [ev1, ev2] {
            match ev {
                AttachEvent::Event { event: AgentEvent::Text(t) } => assert_eq!(t, "shared"),
                other => panic!("expected Event(Text), got {:?}", other),
            }
        }

        j1.abort();
        j2.abort();
    }

    // -------------------------------------------------------------------
    // Test 7: EOF on client side ends the session
    // -------------------------------------------------------------------
    #[tokio::test]
    async fn test_eof_ends_session() {
        let (_client_r, client_w, join, _bus) = spawn_attach_session("rw");

        tokio::time::sleep(Duration::from_millis(20)).await;

        // Drop the client's writer — server sees EOF on read_line
        drop(client_w);

        let result = timeout(Duration::from_secs(2), join).await;
        assert!(result.is_ok(), "handle_attach did not return within 2s of client EOF");
        assert!(result.unwrap().is_ok(), "handle_attach task panicked on EOF");
    }

    // -------------------------------------------------------------------
    // Test 8: Malformed inbound JSON is ignored (not fatal)
    // -------------------------------------------------------------------
    #[tokio::test]
    async fn test_malformed_inbound_ignored() {
        let (_client_r, mut client_w, join, mut bus) = spawn_attach_session("rw");
        let mut inbound_rx = bus.take_inbound_rx().unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;

        // Send garbage — not a valid AttachInbound
        client_w.write_all(b"{not valid json}\n").await.unwrap();
        client_w.flush().await.unwrap();

        // Should not appear on the inbound channel
        let res = timeout(Duration::from_millis(200), inbound_rx.recv()).await;
        assert!(res.is_err(), "malformed JSON must not produce an Inbound, got {:?}", res);

        // Session should still be alive — send a valid message and confirm it works
        send_inbound(&mut client_w, &AttachInbound::Message { content: "after garbage".into() }).await;
        let received = timeout(Duration::from_secs(2), inbound_rx.recv())
            .await
            .expect("inbound_rx timed out after valid message")
            .expect("inbound_rx closed");
        assert!(matches!(received, Inbound::Message { .. }));

        join.abort();
    }

    // -------------------------------------------------------------------
    // Test 9: MessageHistory is filtered out of the attach stream
    //
    // MessageHistory is a TUI-local sync signal — the driver broadcasts the
    // full Vec<Value> on every auto-save. Forwarding it over NDJSON to every
    // attached client would ship the entire conversation history down the
    // wire every turn. The attach handler must drop it silently.
    // -------------------------------------------------------------------
    #[tokio::test]
    async fn test_message_history_filtered_from_attach_stream() {
        let (mut client_r, _client_w, join, bus) = spawn_attach_session("rw");

        tokio::time::sleep(Duration::from_millis(20)).await;

        // Broadcast a MessageHistory event — should be suppressed
        bus.broadcast(AgentEvent::MessageHistory(vec![
            serde_json::json!({"role": "user", "content": "should not reach client"}),
        ]));

        // Immediately broadcast a Text event — should reach the client
        bus.broadcast(AgentEvent::Text("visible".to_string()));

        // The NEXT event the client reads must be the Text event, not MessageHistory
        let ev = recv_attach_event(&mut client_r).await;
        match ev {
            AttachEvent::Event { event: AgentEvent::Text(t) } => assert_eq!(t, "visible"),
            AttachEvent::Event { event: AgentEvent::MessageHistory(_) } => {
                panic!("MessageHistory leaked to attached client")
            }
            other => panic!("expected Event(Text), got {:?}", other),
        }

        join.abort();
    }
}
