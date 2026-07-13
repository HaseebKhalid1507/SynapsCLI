use super::*;

/// Format uptime duration nicely
pub(crate) fn format_uptime(secs: f64) -> String {
    let secs = secs as u64;
    if secs < 60 { format!("{}s", secs) }
    else if secs < 3600 { format!("{}m {}s", secs / 60, secs % 60) }
    else { format!("{}h {}m", secs / 3600, (secs % 3600) / 60) }
}

/// Print status response in table format
pub(crate) fn print_status_table(agents: Vec<AgentStatusInfo>) {
    if agents.is_empty() {
        println!("No agents configured. Run: watcher init <name>");
        return;
    }
    
    println!("{:<15} {:<10} {:<10} {:<10} {:<10} {:<12}", "AGENT", "TRIGGER", "STATUS", "SESSION", "UPTIME", "COST TODAY");
    println!("{}", "─".repeat(80));
    
    for agent in agents {
        let uptime = agent.uptime_secs.map(format_uptime).unwrap_or_else(|| "—".to_string());
        let session = if agent.session_count > 0 { 
            format!("#{}", agent.session_count) 
        } else { 
            "—".to_string() 
        };
        let cost = format!("${:.2}/${:.2}", agent.cost_today, agent.cost_limit);
        
        println!("{:<15} {:<10} {:<10} {:<10} {:<10} {:<12}",
            agent.name,
            agent.trigger,
            agent.status,
            session,
            uptime,
            cost
        );
    }
}

/// Print detailed agent status
pub(crate) fn print_agent_detail(info: AgentStatusInfo) {
    println!("Agent: {}", info.name);
    println!("Trigger: {}", info.trigger);
    
    let session_str = if info.session_count > 0 {
        format!("{} (session #{})", info.status, info.session_count)
    } else {
        info.status
    };
    println!("Status: {}", session_str);
    println!("Model: {}", info.model);
    
    if let Some(pid) = info.pid {
        println!("PID: {}", pid);
    }
    if let Some(uptime) = info.uptime_secs {
        println!("Uptime: {}", format_uptime(uptime));
    }
    
    println!("Sessions: {} (total) / {} (today)", info.total_sessions, 
        if info.session_count > 0 { info.session_count } else { 0 });
    println!("Cost: ${:.2} today / ${:.2} limit", info.cost_today, info.cost_limit);
    
    // Format tokens with commas
    let tokens_formatted = format_number_with_commas(info.tokens_today);
    println!("Tokens: {} today", tokens_formatted);
    
    println!("Crashes: {}", info.consecutive_crashes);
}

/// Format numbers with commas for readability
pub(crate) fn format_number_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.insert(0, ',');
        }
        result.insert(0, c);
    }
    result
}

pub(crate) fn format_log_entry(entry: &str) -> Option<String> {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(entry) {
        let ts_str = json["ts"].as_str().unwrap_or("??:??:??");
        let timestamp = if let Some(time_part) = ts_str.split('T').nth(1) {
            time_part.split('.').next().unwrap_or(time_part)
        } else {
            ts_str
        };

        let log_type = json["type"].as_str().unwrap_or("unknown");
        
        match log_type {
            "boot" => {
                let session = json["session"].as_u64().unwrap_or(0);
                let model = json["model"].as_str().unwrap_or("unknown");
                let trigger = json["trigger"].as_str().unwrap_or("unknown");
                Some(format!("[{}] BOOT session #{} (model: {}, trigger: {})", timestamp, session, model, trigger))
            }
            "tool_start" => {
                let name = json["name"].as_str().unwrap_or("unknown");
                let call_num = json["call_num"].as_u64().unwrap_or(0);
                Some(format!("[{}] TOOL {} (#{}) ", timestamp, name, call_num))
            }
            "tool_result" => {
                let preview = json["preview"].as_str().unwrap_or("").chars().take(80).collect::<String>();
                Some(format!("[{}]   → {}", timestamp, preview))
            }
            "usage" => {
                let input_tokens = json["input_tokens"].as_u64().unwrap_or(0);
                let output_tokens = json["output_tokens"].as_u64().unwrap_or(0);
                let total_tokens = json["total_tokens"].as_u64().unwrap_or(0);
                let cost = json["cost"].as_f64().unwrap_or(0.0);
                Some(format!("[{}] USAGE +{}/+{} tokens (total: {}, cost: ${:.4})", 
                    timestamp, input_tokens, output_tokens, total_tokens, cost))
            }
            "text" => {
                let length = json["length"].as_u64().unwrap_or(0);
                let preview = json["preview"].as_str().unwrap_or("").chars().take(80).collect::<String>();
                Some(format!("[{}] TEXT {} chars: {}", timestamp, length, preview))
            }
            "exit" => {
                let reason = json["reason"].as_str().unwrap_or("unknown");
                let total_tokens = json["total_tokens"].as_u64().unwrap_or(0);
                let total_cost = json["total_cost"].as_f64().unwrap_or(0.0);
                let tool_calls = json["tool_calls"].as_u64().unwrap_or(0);
                let duration_secs = json["duration_secs"].as_u64().unwrap_or(0);
                Some(format!("[{}] EXIT {} ({} tokens, ${:.2}, {} tool calls, {}s)", 
                    timestamp, reason, total_tokens, total_cost, tool_calls, duration_secs))
            }
            _ => Some(format!("[{}] {}: {}", timestamp, log_type.to_uppercase(), entry))
        }
    } else {
        None
    }
}

pub(crate) fn find_latest_session_file(logs_dir: &Path) -> Result<PathBuf, String> {
    let mut max_session = 0;
    let mut found_any = false;
    
    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("session-") && name_str.ends_with(".jsonl") {
                found_any = true;
                if let Ok(num) = name_str.trim_start_matches("session-").trim_end_matches(".jsonl").parse::<u64>() {
                    if num > max_session {
                        max_session = num;
                    }
                }
            }
        }
    }
    
    if !found_any {
        return Err("No session logs found".to_string());
    }
    
    Ok(logs_dir.join(format!("session-{:03}.jsonl", max_session)))
}

pub(crate) async fn show_logs(name: &str, follow: bool, session_num: Option<u64>, last_n: Option<usize>) -> Result<(), String> {
    let logs_dir = watcher_dir().join(name).join("logs");
    
    if !logs_dir.exists() {
        return Err(format!("Agent '{}' has no logs directory", name));
    }

    let log_file = if let Some(session) = session_num {
        logs_dir.join(format!("session-{:03}.jsonl", session))
    } else {
        find_latest_session_file(&logs_dir)?
    };

    if !log_file.exists() {
        return Err(format!("Log file {:?} does not exist", log_file));
    }

    if follow {
        // For follow mode, use current.log if available
        let current_log = logs_dir.join("current.log");
        let follow_path = if current_log.exists() {
            if let Ok(contents) = std::fs::read_to_string(&current_log) {
                PathBuf::from(contents.trim())
            } else {
                log_file
            }
        } else {
            log_file
        };

        // Initial read
        if let Ok(contents) = std::fs::read_to_string(&follow_path) {
            for line in contents.lines() {
                if let Some(formatted) = format_log_entry(line) {
                    println!("{}", formatted);
                }
            }
        }

        // Poll for new lines
        let mut last_size = std::fs::metadata(&follow_path).map(|m| m.len()).unwrap_or(0);
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            
            if let Ok(metadata) = tokio::fs::metadata(&follow_path).await {
                let current_size = metadata.len();
                if current_size > last_size {
                    if let Ok(contents) = tokio::fs::read_to_string(&follow_path).await {
                        let new_content = &contents[(last_size as usize)..];
                        for line in new_content.lines() {
                            if !line.trim().is_empty() {
                                if let Some(formatted) = format_log_entry(line) {
                                    println!("{}", formatted);
                                }
                            }
                        }
                        last_size = current_size;
                    }
                }
            }
        }
    } else {
        // Read and display log file
        let contents = tokio::fs::read_to_string(&log_file).await
            .map_err(|e| format!("Failed to read log file: {}", e))?;
        
        let mut lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        
        if let Some(n) = last_n {
            if lines.len() > n {
                lines = lines[(lines.len() - n)..].to_vec();
            }
        }

        for line in lines {
            if let Some(formatted) = format_log_entry(line) {
                println!("{}", formatted);
            }
        }
    }

    Ok(())
}
