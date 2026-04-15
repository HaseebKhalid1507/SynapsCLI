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
async fn handle_attach(
    mut buf_reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    bus_handle: synaps_cli::BusHandle,
    mode: String,
) {
    let mut event_rx = bus_handle.subscribe();
    let inbound_tx = bus_handle.inbound();
    let read_only = mode == "ro";

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Ok(e) => {
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