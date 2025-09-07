use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::net::{Ipv4Addr, Ipv6Addr};
use urlencoding::encode;
use uuid::{Builder, Uuid, Variant, Version};
use worker::*;
mod lib1;
// ===================================================================================
// 配置结构体
// ===================================================================================

#[derive(Debug)]
struct AppConfig {
    sub_path: String,
    uuid: String,
    txt_url: String,
    nat64_prefix: Option<String>,
    doh_address: String,
    proxy_ip: Option<String>,
    v2ray_split: (String, String),
    vless_split: (String, String),
    clash_split: (String, String),
}

impl AppConfig {
    // 从环境变量初始化配置
    fn from_env(env: &Env, sub_path_from_url: &str) -> Result<Self> {
        let sub_path = env
            .var("SUB_PATH")
            .map(|v| v.to_string())
            .unwrap_or_else(|_| sub_path_from_url.to_string());

        Ok(AppConfig {
            uuid: generate_uuid(&sub_path),
            sub_path,
            txt_url: env
                .var("TXT_URL")
                .map(|v| v.to_string())
                .unwrap_or_else(|_| {
                    "https://raw.githubusercontent.com/ImLTHQ/edgetunnel/main/AutoTest.txt"
                        .to_string()
                }),
            nat64_prefix: env.var("NAT64").ok().map(|v| v.to_string()),
            doh_address: env
                .var("DOH")
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "1.1.1.1".to_string()),
            proxy_ip: env.var("PROXY_IP").ok().map(|v| v.to_string()),
            v2ray_split: ("v2".to_string(), "ray".to_string()),
            vless_split: ("vl".to_string(), "ess".to_string()),
            clash_split: ("cla".to_string(), "sh".to_string()),
        })
    }
}

// ===================================================================================
// 主入口和路由
// ===================================================================================

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // 设置 panic hook 以便更好地调试
    console_error_panic_hook::set_once();

    let router = Router::new();
    router
        .get_async("/:sub_path", handle_subscription)
        .get_async("/:sub_path/:client", |req, ctx| async move {
            let sub_path = ctx.param("sub_path").unwrap();
            let client = ctx.param("client").unwrap();
            let config = AppConfig::from_env(&ctx.env, sub_path)?;
            let host = req.headers().get("host")?.unwrap_or_default();

            let v2ray_path = format!("{}{}", config.v2ray_split.0, config.v2ray_split.1);
            let clash_path = format!("{}{}", config.clash_split.0, config.clash_split.1);

            if client == &v2ray_path {
                generate_v2ray_config(&host, &config).await
            } else if client == &clash_path {
                generate_clash_config(&host, &config).await
            } else {
                Response::error("Not Found", 404)
            }
        })
        .on_async("/*", |req, ctx| async move {
            // 捕获所有其他路径用于WebSocket或反向代理
            let upgrade = req
                .headers()
                .get("upgrade")?
                .unwrap_or_default()
                .to_lowercase();
            if upgrade == "websocket" {
                return handle_websocket_upgrade(req, ctx.env).await;
            }

            let url = req.url()?;
            let sub_path = AppConfig::from_env(&ctx.env, "default")?.sub_path;
            let proxy_prefix = format!("/{}", encode(&sub_path));

            if url.path().starts_with(&proxy_prefix) {
                return handle_reverse_proxy(req, &proxy_prefix).await;
            }

            Response::error("Not Found", 404)
        })
        .run(req, env)
        .await
}

async fn handle_subscription(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let sub_path = ctx.param("sub_path").unwrap();
    let config = AppConfig::from_env(&ctx.env, sub_path)?;

    let user_agent = req
        .headers()
        .get("user-agent")?
        .unwrap_or_default()
        .to_lowercase();
    let host = req.headers().get("host")?.unwrap_or_default();

    let v2ray_ua_keyword = format!("{}{}", config.v2ray_split.0, config.v2ray_split.1);
    let clash_ua_keyword = format!("{}{}", config.clash_split.0, config.clash_split.1);

    if user_agent.contains(&v2ray_ua_keyword) {
        generate_v2ray_config(&host, &config).await
    } else if user_agent.contains(&clash_ua_keyword) {
        generate_clash_config(&host, &config).await
    } else {
        tips_page(&config)
    }
}

// ===================================================================================
// WebSocket 和 VLESS 处理
// ===================================================================================

async fn handle_websocket_upgrade(req: Request, env: Env) -> Result<Response> {
    let pair = WebSocketPair::new()?;
    let server = pair.server;
    server.accept()?;

    let config = AppConfig::from_env(&env, "default")?;
    let protocol = req
        .headers()
        .get("sec-websocket-protocol")?
        .unwrap_or_default();

    // 在新任务中处理WebSocket连接
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = handle_socket_stream(server, protocol, config).await {
            console_error!("WebSocket stream error: {}", e);
        }
    });

    Response::from_websocket(pair.client)
}

async fn handle_socket_stream(ws: WebSocket, protocol: String, config: AppConfig) -> Result<()> {
    // 解码 sec-websocket-protocol 头部作为VLESS头部
    let vless_header = decode_protocol_header(&protocol)?;

    let (uuid_from_header, dest_addr, initial_data) = parse_vless_header(&vless_header)?;

    if uuid_from_header.to_string() != config.uuid {
        console_error!(
            "Invalid UUID. Expected: {}, Got: {}",
            config.uuid,
            uuid_from_header
        );
        ws.close(Some(1000), Some("Invalid UUID"))?;
        return Ok(());
    }

    // 建立到目标的TCP连接
    let mut tcp_stream = establish_tcp_connection(&dest_addr, &config).await?;

    let (mut ws_tx, mut ws_rx) = ws.split();

    // 发送初始WebSocket消息
    ws_tx.send(Message::Bytes(vec![0, 0])).await?;

    // 将初始数据写入TCP流
    tcp_stream.write_all(&initial_data).await?;

    // 创建双向数据管道
    let (mut tcp_reader, mut tcp_writer) = tcp_stream.split();

    let ws_to_tcp = async {
        while let Some(msg) = ws_rx.next().await {
            match msg? {
                Message::Bytes(data) => {
                    if tcp_writer.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Message::Text(_) => { /* VLESS 不使用文本消息 */ }
            }
        }
        Ok(())
    };

    let tcp_to_ws = async {
        let mut buffer = vec![0; 4096];
        loop {
            match tcp_reader.read(&mut buffer).await {
                Ok(Some(n)) => {
                    if ws_tx
                        .send(Message::Bytes(buffer[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                _ => break, // 读取错误或流结束
            }
        }
    };

    // 等待任一管道结束
    futures_util::future::select(Box::pin(ws_to_tcp), Box::pin(tcp_to_ws)).await;

    console_log!("Pipe finished. Closing connections.");
    ws.close(Some(1000), Some("Closing")).await?;

    Ok(())
}

fn decode_protocol_header(protocol: &str) -> Result<Vec<u8>> {
    let sanitized = protocol.replace('-', "+").replace('_', "/");
    B64_STANDARD
        .decode(sanitized)
        .map_err(|e| worker::Error::RustError(format!("Base64 Decode Error: {}", e)))
}

fn parse_vless_header(data: &[u8]) -> Result<(Uuid, DestinationAddress, Vec<u8>)> {
    if data.len() < 20 {
        // 最小长度检查
        return Err(worker::Error::RustError("VLESS header too short".into()));
    }

    // 版本 (1 byte) + UUID (16 bytes)
    let uuid = Uuid::from_slice(&data[1..17]).unwrap();

    let addons_len = data[17] as usize;
    let addr_start = 18 + addons_len;

    if data.len() < addr_start + 3 {
        // 至少需要 端口(2) + 地址类型(1)
        return Err(worker::Error::RustError(
            "Invalid VLESS address section".into(),
        ));
    }

    let port = u16::from_be_bytes([data[addr_start], data[addr_start + 1]]);
    let addr_type = data[addr_start + 2];

    let mut current_pos = addr_start + 3;
    let host: String;

    match addr_type {
        1 => {
            // IPv4
            if data.len() < current_pos + 4 {
                return Err(worker::Error::RustError("Incomplete IPv4 address".into()));
            }
            host = Ipv4Addr::new(
                data[current_pos],
                data[current_pos + 1],
                data[current_pos + 2],
                data[current_pos + 3],
            )
            .to_string();
            current_pos += 4;
        }
        2 => {
            // Domain
            if data.len() < current_pos + 1 {
                return Err(worker::Error::RustError("Missing domain length".into()));
            }
            let domain_len = data[current_pos] as usize;
            current_pos += 1;
            if data.len() < current_pos + domain_len {
                return Err(worker::Error::RustError("Incomplete domain name".into()));
            }
            host = String::from_utf8(data[current_pos..current_pos + domain_len].to_vec())?;
            current_pos += domain_len;
        }
        3 => {
            // IPv6
            if data.len() < current_pos + 16 {
                return Err(worker::Error::RustError("Incomplete IPv6 address".into()));
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&data[current_pos..current_pos + 16]);
            host = Ipv6Addr::from(octets).to_string();
            current_pos += 16;
        }
        _ => {
            return Err(worker::Error::RustError(format!(
                "Unsupported address type: {}",
                addr_type
            )))
        }
    }

    let dest_addr = DestinationAddress {
        host,
        port,
        addr_type,
    };
    let initial_data = data[current_pos..].to_vec();

    Ok((uuid, dest_addr, initial_data))
}

// ===================================================================================
// TCP 连接逻辑
// ===================================================================================

struct DestinationAddress {
    host: String,
    port: u16,
    addr_type: u8,
}

async fn establish_tcp_connection(dest: &DestinationAddress, config: &AppConfig) -> Result<Socket> {
    // 1. 尝试直连
    if let Ok(socket) = Socket::connect(format!("{}:{}", &dest.host, dest.port)) {
        console_log!("Direct connection successful to {}", &dest.host);
        return Ok(socket);
    }
    console_warn!("Direct connection to {} failed", &dest.host);

    // 2. 尝试 NAT64
    if let Some(nat64_prefix) = &config.nat64_prefix {
        let ipv4_to_convert = if dest.addr_type == 1 {
            Some(dest.host.clone())
        } else if dest.addr_type == 2 {
            resolve_domain_to_ipv4(&dest.host, &config.doh_address)
                .await
                .ok()
        } else {
            None
        };

        if let Some(ipv4) = ipv4_to_convert {
            if let Ok(nat64_addr) = convert_ipv4_to_nat64(&ipv4, nat64_prefix) {
                if let Ok(socket) = Socket::connect(format!("{}:{}", nat64_addr, dest.port)) {
                    console_log!("NAT64 connection successful to {}", nat64_addr);
                    return Ok(socket);
                }
                console_warn!("NAT64 connection to {} failed", nat64_addr);
            }
        }
    }

    // 3. 尝试反代IP
    if let Some(proxy_ip) = &config.proxy_ip {
        if let Ok(socket) = Socket::connect(proxy_ip.clone()) {
            console_log!("Proxy connection successful to {}", proxy_ip);
            return Ok(socket);
        }
        console_warn!("Proxy connection to {} failed", proxy_ip);
    }

    Err(worker::Error::RustError(
        "All connection methods failed".into(),
    ))
}

fn convert_ipv4_to_nat64(ipv4: &str, prefix: &str) -> Result<String> {
    let clean_prefix = prefix.trim_end_matches('/');
    let ipv4_addr: Ipv4Addr = ipv4
        .parse()
        .map_err(|_| worker::Error::RustError("Invalid IPv4 format".into()))?;
    let octets = ipv4_addr.octets();
    Ok(format!(
        "[{}:{:02x}{:02x}:{:02x}{:02x}]",
        clean_prefix, octets[0], octets[1], octets[2], octets[3]
    ))
}

#[derive(Deserialize)]
struct DohAnswer {
    data: String,
    #[serde(rename = "type")]
    type_code: u16,
}
#[derive(Deserialize)]
struct DohResponse {
    #[serde(rename = "Answer")]
    answer: Option<Vec<DohAnswer>>,
}

async fn resolve_domain_to_ipv4(domain: &str, doh_address: &str) -> Result<String> {
    let url = format!("https://{}/dns-query?name={}&type=A", doh_address, domain);
    let mut headers = Headers::new();
    headers.set("accept", "application/dns-json")?;

    let mut res = Fetch::Request(Request::new_with_init(
        &url,
        RequestInit::new().with_headers(headers),
    )?)
    .send()
    .await?;

    if !res.status_code() == 200 {
        return Err(worker::Error::RustError(format!(
            "DoH query failed with status {}",
            res.status_code()
        )));
    }

    let doh_res: DohResponse = res.json().await?;
    doh_res
        .answer
        .and_then(|answers| {
            answers
                .into_iter()
                .find(|a| a.type_code == 1)
                .map(|a| a.data)
        })
        .ok_or_else(|| worker::Error::RustError("No A record found in DoH response".into()))
}

// ===================================================================================
// 订阅配置生成
// ===================================================================================

async fn get_preferred_list(txt_url: &str) -> Result<Vec<String>> {
    if txt_url.trim().is_empty() {
        return Ok(vec![]);
    }
    let mut response = Fetch::Url(Url::parse(txt_url)?).send().await?;
    if !response.status_code() == 200 {
        return Err(worker::Error::RustError(format!(
            "Failed to fetch preferred list: status {}",
            response.status_code()
        )));
    }
    let text = response.text().await?;
    Ok(text
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

fn process_nodes(mut preferred_list: Vec<String>, host_name: &str) -> Vec<(String, u16, String)> {
    preferred_list.insert(0, format!("{}#原生节点", host_name));

    preferred_list
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let parts: Vec<&str> = line.splitn(2, '#').collect();
            let address_part = parts[0];
            let name = parts
                .get(1)
                .map_or_else(|| format!("节点 {}", i + 1), |s| s.to_string());

            let mut addr_parts: Vec<&str> = address_part.rsplitn(2, ':').collect();
            let port_str = addr_parts.get(0).unwrap_or(&"");
            let port: u16 = port_str.parse().unwrap_or(443);
            let address = if addr_parts.len() > 1 {
                addr_parts.get(1).unwrap_or(&"").to_string()
            } else {
                port_str.to_string()
            };

            (address, port, name)
        })
        .collect()
}

async fn generate_v2ray_config(host: &str, config: &AppConfig) -> Result<Response> {
    let preferred_list = get_preferred_list(&config.txt_url)
        .await
        .unwrap_or_default();
    let nodes = process_nodes(preferred_list, host);

    let links: Vec<String> = nodes.iter().map(|(address, port, name)| {
        format!("{}://{}@{}:{}?encryption=none&security=tls&sni={}&fp=chrome&type=ws&host={}&path=%2F%3Fed%3D2560#{}",
            format!("{}{}", config.vless_split.0, config.vless_split.1),
            config.uuid,
            address,
            port,
            host,
            host,
            encode(name)
        )
    }).collect();

    Response::from_bytes(links.join("\n").into_bytes())?.with_headers(Headers::from_iter(vec![(
        "Content-Type",
        "text/plain;charset=utf-8",
    )]))
}

async fn generate_clash_config(host: &str, config: &AppConfig) -> Result<Response> {
    let preferred_list = get_preferred_list(&config.txt_url)
        .await
        .unwrap_or_default();
    let nodes = process_nodes(preferred_list, host);

    let proxies: Vec<String> = nodes
        .iter()
        .map(|(address, port, name)| {
            format!(
                r#"  - name: {}
    type: {}
    server: {}
    port: {}
    uuid: {}
    udp: true
    tls: true
    sni: {}
    network: ws
    ws-opts:
      path: "/?ed=2560"
      headers:
        Host: {}"#,
                name,
                format!("{}{}", config.vless_split.0, config.vless_split.1),
                address,
                port,
                config.uuid,
                host,
                host
            )
        })
        .collect();

    let proxy_names: Vec<String> = nodes
        .iter()
        .map(|(_, _, name)| format!("    - {}", name))
        .collect();

    let yaml_content = format!(
        r#"proxies:
{}

proxy-groups:
  - name: 节点选择
    type: select
    proxies:
{}
  - name: 自动选择
    type: url-test
    url: https://www.google.com/generate_204
    interval: 300
    proxies:
{}

rules:
  - MATCH,节点选择
"#,
        proxies.join("\n"),
        proxy_names.join("\n"),
        proxy_names.join("\n")
    );

    Response::from_bytes(yaml_content.into_bytes())?.with_headers(Headers::from_iter(vec![(
        "Content-Type",
        "text/plain;charset=utf-8",
    )]))
}

// ===================================================================================
// 工具函数和页面
// ===================================================================================

fn tips_page(config: &AppConfig) -> Result<Response> {
    let html = format!(
        r#"
<!DOCTYPE html>
<html>
<head>
    <title>订阅-{}</title>
    <meta charset="utf-8">
    <style>
      body {{ font-size: 25px; text-align: center; font-family: sans-serif; }}
      strong {{ color: #4a4a4a; }}
    </style>
</head>
<body>
    <strong>请把链接导入 {} 或 {} 客户端</strong>
</body>
</html>
"#,
        config.sub_path,
        format!("{}{}", config.clash_split.0, config.clash_split.1),
        format!("{}{}", config.v2ray_split.0, config.v2ray_split.1)
    );
    Response::from_html(html)
}

fn generate_uuid(text: &str) -> String {
    let mut bytes = [0u8; 16];
    let hash = format!("{:x}", md5::compute(text.as_bytes()));
    let hash_bytes = hash.as_bytes();

    // 从MD5哈希创建伪随机字节，以确保一致性
    for i in 0..16 {
        let hex_pair = &hash_bytes[i * 2..i * 2 + 2];
        bytes[i] = u8::from_str_radix(std::str::from_utf8(hex_pair).unwrap(), 16).unwrap();
    }

    // 设置UUID版本为4和变体为RFC 4122
    let mut builder = Builder::from_bytes(bytes);
    builder.set_version(Version::Random);
    builder.set_variant(Variant::RFC4122);

    builder.into_uuid().to_string()
}

async fn handle_reverse_proxy(req: Request, prefix: &str) -> Result<Response> {
    let url = req.url()?;
    let target_url = &url.path()[prefix.len()..];

    if target_url.is_empty() {
        return Response::error("Target URL for proxy is empty", 400);
    }

    let mut new_headers = req.headers().clone();
    new_headers.delete("host")?; // Let Fetch API set the correct host

    let proxy_req_init = RequestInit::new()
        .with_method(req.method())
        .with_headers(new_headers)
        .with_body(req.bytes().await.ok().map(Into::into));

    let proxy_req = Request::new_with_init(target_url, &proxy_req_init)?;

    Fetch::Request(proxy_req).send().await
}
