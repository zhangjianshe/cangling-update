use anyhow::{bail, Context, Result};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};

/// Run a foreground TCP proxy until Ctrl+C terminates the process.
pub async fn run(
    listen_host: &str,
    listen_port: u16,
    target_host: &str,
    target_port: u16,
) -> Result<()> {
    validate_port("监听", listen_port)?;
    validate_port("目标", target_port)?;

    let listen = format_address(listen_host, listen_port);
    let target = format_address(target_host, target_port);
    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("无法监听 {listen}"))?;

    eprintln!("TCP 端口转发已启动：{listen} -> {target}；按 Ctrl+C 停止");
    loop {
        let (incoming, peer) = listener
            .accept()
            .await
            .with_context(|| format!("无法接受 {listen} 上的连接"))?;
        let target = target.clone();
        tokio::spawn(async move {
            if let Err(error) = forward_connection(incoming, &target).await {
                eprintln!("端口转发失败（{peer} -> {target}）：{error:#}");
            }
        });
    }
}

async fn forward_connection(mut incoming: TcpStream, target: &str) -> Result<()> {
    let mut outgoing = TcpStream::connect(target)
        .await
        .with_context(|| format!("无法连接目标 {target}"))?;
    copy_bidirectional(&mut incoming, &mut outgoing)
        .await
        .context("双向转发数据失败")?;
    Ok(())
}

fn validate_port(label: &str, port: u16) -> Result<()> {
    if port == 0 {
        bail!("{label}端口必须在 1 到 65535 之间");
    }
    Ok(())
}

fn format_address(host: &str, port: u16) -> String {
    let host = host.trim();
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn formats_ipv4_and_ipv6_addresses() {
        assert_eq!(format_address("127.0.0.1", 22), "127.0.0.1:22");
        assert_eq!(format_address("::1", 22), "[::1]:22");
        assert_eq!(format_address("[::1]", 22), "[::1]:22");
    }

    #[test]
    fn rejects_zero_port() {
        assert!(validate_port("监听", 0).is_err());
        assert!(validate_port("目标", 0).is_err());
    }

    #[tokio::test]
    async fn forwards_data_in_both_directions() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target.local_addr().unwrap().to_string();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();

        let echo = tokio::spawn(async move {
            let (mut stream, _) = target.accept().await.unwrap();
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });
        let relay = tokio::spawn(async move {
            let (incoming, _) = proxy.accept().await.unwrap();
            forward_connection(incoming, &target_address).await.unwrap();
        });

        let mut client = TcpStream::connect(proxy_address).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut response = [0_u8; 4];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        drop(client);

        echo.await.unwrap();
        relay.await.unwrap();
    }
}
