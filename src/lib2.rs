use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::io::Cursor;
use uuid::Uuid;
use worker::*;

#[event(fetch)]
pub async fn _main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let upgrade = req.headers().get("Upgrade")?.unwrap_or_default();

    if upgrade.to_lowercase() == "websocket" {
        return handle_websocket(req, env).await;
    }

    // 非 WS 请求：返回订阅或 404
    Response::error("Not Found", 404);
    console_error_panic_hook::set_once();
    Response::ok("Hello World!")
}

async fn handle_websocket(req: Request, env: Env) -> Result<Response> {
    let ws = WebSocketPair::new()?;
    let client = ws.client;
    let server = ws.server;

    server.accept()?;

    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = handle_socket(server, env).await {
            console_log!("WebSocket error: {:?}", e);
        }
    });

    Response::from_websocket(client)
}

async fn handle_socket(ws: WebSocket, env: Env) -> Result<()> {
    let Some(proto) = ws.headers().get("sec-websocket-protocol")? else {
        return Err(Error::RustError("Missing protocol".into()));
    };

    let decoded = URL_SAFE_NO_PAD
        .decode(proto.replace("-", "+").replace("_", "/"))
        .map_err(|_| Error::RustError("Invalid base64".into()))?;

    let uuid = env.secret("UUID")?.to_string();

    if !validate_uuid(&decoded[1..17], &uuid) {
        return Err(Error::RustError("Invalid UUID".into()));
    }

    let addr_type = decoded[17];
    let port_offset = 18 + (decoded[18] as usize) + 1;
    let port = u16::from_be_bytes([decoded[port_offset], decoded[port_offset + 1]]);

    let addr_offset = port_offset + 2;
    let addr = match addr_type {
        1 => {
            let octets = &decoded[addr_offset..addr_offset + 4];
            format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3])
        }
        2 => {
            let len = decoded[addr_offset] as usize;
            String::from_utf8_lossy(&decoded[addr_offset + 1..addr_offset + 1 + len]).into_owned()
        }
        3 => {
            // IPv6
            let mut cursor = Cursor::new(&decoded[addr_offset..addr_offset + 16]);
            let mut segments = Vec::new();
            for _ in 0..8 {
                segments.push(cursor.read_u16::<BigEndian>()?.to_string());
            }
            segments.join(":")
        }
        _ => return Err(Error::RustError("Unsupported address type".into())),
    };

    let initial = &decoded[addr_offset + addr.len()..];

    // 建立 TCP 连接
    let socket = Socket::builder()
        .secure_transport(SecureTransport::Off)
        .connect(&addr, port)
        .await?;

    let (mut tx, mut rx) = socket.split();
    let mut ws_stream = ws.stream()?;

    // 发送初始数据
    if !initial.is_empty() {
        tx.write_all(initial).await?;
    }

    // 双向转发
    let tx_task = async {
        while let Some(msg) = ws_stream.next().await {
            if let Ok(Message::Binary(bin)) = msg {
                tx.write_all(&bin).await?;
            }
        }
        Ok::<_, Error>(())
    };

    let rx_task = async {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = rx.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            ws.send_with_bytes(&buf[..n]).await?;
        }
        Ok::<_, Error>(())
    };

    futures::try_join!(tx_task, rx_task)?;
    Ok(())
}

fn validate_uuid(slice: &[u8], expected: &str) -> bool {
    if slice.len() != 16 {
        return false;
    }
    let uuid = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        slice[0], slice[1], slice[2], slice[3],
        slice[4], slice[5], slice[6], slice[7],
        slice[8], slice[9], slice[10], slice[11], slice[12], slice[13], slice[14], slice[15]
    );
    uuid == expected
}
