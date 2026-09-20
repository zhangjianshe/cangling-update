use crate::docker::Docker;
use crate::error::AppError;
use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug, Deserialize)]
pub struct ExecQuery {
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct ClientMsg {
    #[serde(rename = "type")]
    kind: String,
    cols: Option<u16>,
    rows: Option<u16>,
}

pub async fn run_exec_socket(
    mut socket: WebSocket,
    docker: Docker,
    dir: std::path::PathBuf,
    service: String,
    query: ExecQuery,
) {
    let cols = query.cols.filter(|c| *c >= 2).unwrap_or(80);
    let rows = query.rows.filter(|r| *r >= 1).unwrap_or(24);
    let (program, args) = match docker.compose_exec_argv(&service).await {
        Ok(v) => v,
        Err(err) => {
            let _ = socket
                .send(Message::Text(format!("无法进入容器：{err:#}\r\n").into()))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    if let Err(err) = pump_command(
        socket,
        &program,
        &args,
        &dir,
        cols,
        rows,
        &format!("已连接 {service}，正在进入容器…\r\n"),
        None,
    )
    .await
    {
        tracing::warn!("compose exec {service}: {err:#}");
    }
}

pub async fn run_command_socket(
    socket: WebSocket,
    program: String,
    args: Vec<String>,
    dir: std::path::PathBuf,
    query: ExecQuery,
    banner: String,
    password: Option<String>,
) {
    let cols = query.cols.filter(|c| *c >= 2).unwrap_or(80);
    let rows = query.rows.filter(|r| *r >= 1).unwrap_or(24);
    if let Err(err) =
        pump_command(socket, &program, &args, &dir, cols, rows, &banner, password).await
    {
        tracing::warn!("host terminal: {err:#}");
    }
}

async fn pump_command(
    mut socket: WebSocket,
    program: &str,
    args: &[String],
    dir: &Path,
    cols: u16,
    rows: u16,
    banner: &str,
    password: Option<String>,
) -> Result<()> {
    let session = match spawn_pty(program, args, dir, cols, rows) {
        Ok(s) => s,
        Err(err) => {
            let _ = socket
                .send(Message::Text(format!("启动终端失败：{err:#}\r\n").into()))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return Ok(());
        }
    };

    let _ = socket.send(Message::Text(banner.to_string().into())).await;

    let mut output_rx = session.output;
    let password_deadline = Instant::now() + Duration::from_secs(15);
    let mut password_sent = false;
    let mut prompt_tail = String::new();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if text.starts_with('{') {
                            if let Ok(msg) = serde_json::from_str::<ClientMsg>(&text) {
                                if msg.kind == "resize" {
                                    if let (Some(c), Some(r)) = (msg.cols, msg.rows) {
                                        if c >= 2 && r >= 1 {
                                            let _ = session.master.lock().unwrap().resize(PtySize {
                                                cols: c,
                                                rows: r,
                                                pixel_width: 0,
                                                pixel_height: 0,
                                            });
                                        }
                                    }
                                    continue;
                                }
                            }
                        }
                        if session.input.send(text.as_bytes().to_vec()).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(bin))) => {
                        if session.input.send(bin.to_vec()).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                }
            }
            chunk = output_rx.recv() => {
                match chunk {
                    Some(bytes) if !bytes.is_empty() => {
                        if !password_sent && password.is_some() && Instant::now() <= password_deadline {
                            prompt_tail.push_str(&String::from_utf8_lossy(&bytes).to_ascii_lowercase());
                            if prompt_tail.len() > 512 {
                                prompt_tail = prompt_tail
                                    .chars()
                                    .rev()
                                    .take(512)
                                    .collect::<String>()
                                    .chars()
                                    .rev()
                                    .collect();
                            }
                            if prompt_tail.contains("password:") {
                                if let Some(secret) = password.as_deref() {
                                    let _ = session.input.send(format!("{secret}\n").into_bytes());
                                }
                                password_sent = true;
                                prompt_tail.clear();
                            }
                        }
                        if socket.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    let _ = session.input.send(Vec::new());
    let _ = socket.send(Message::Close(None)).await;
    Ok(())
}

struct PtySession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    input: mpsc::UnboundedSender<Vec<u8>>,
    output: mpsc::UnboundedReceiver<Vec<u8>>,
}

fn spawn_pty(
    program: &str,
    args: &[String],
    dir: &Path,
    cols: u16,
    rows: u16,
) -> Result<PtySession> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(program);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.cwd(dir);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let mut writer = pair.master.take_writer().context("take pty writer")?;
    let master = Arc::new(Mutex::new(pair.master));

    let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    thread::Builder::new()
        .name("cangling-pty-out".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if out_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })?;

    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    thread::Builder::new()
        .name("cangling-pty-in".into())
        .spawn(move || {
            while let Some(bytes) = in_rx.blocking_recv() {
                if bytes.is_empty() {
                    break;
                }
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        })?;

    thread::Builder::new()
        .name("cangling-pty-wait".into())
        .spawn(move || {
            let _ = child.wait();
        })?;

    Ok(PtySession {
        master,
        input: in_tx,
        output: out_rx,
    })
}

pub fn require_service_name(name: &str) -> Result<(), AppError> {
    crate::docker::validate_service_name(name).map_err(|e| AppError::bad(e.to_string()))
}
