use base64::{engine::general_purpose, Engine as _};
use std::convert::TryInto;
use std::str;
use futures_util::AsyncWriteExt;
// use futures_util::{AsyncWrite, AsyncWriteExt};
use worker::WebsocketEvent::Message;
use worker::*;
use worker::wasm_bindgen::JsValue;

#[derive(Debug, Clone)]
struct Config {
    sub_path: String,
    uuid: String,
    txt_url: String,
    nat64_prefix: String,
    doh_address: String,
    proxy_ip: String,
    vless_split_1: String,
    vless_split_2: String,
    clash_split_1: String,
    clash_split_2: String,
}

impl Config {
    fn vless(&self) -> String {
        format!("{}{}", self.vless_split_1, self.vless_split_2)
    }
    fn clash(&self) -> String {
        format!("{}{}", self.clash_split_1, self.clash_split_2)
    }

    fn from_env(env: &Env, sub_path_from_url: &str) -> Result<Self> {
        let sub_path = env
            .var("SUB_PATH")
            .map(|v| v.to_string())
            .unwrap_or_else(|_| sub_path_from_url.to_string());

        Ok(Config {
            sub_path,
            ..Default::default()
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sub_path: "订阅路径".to_string(),
            uuid: String::new(),
            txt_url: "https://raw.githubusercontent.com/ImLTHQ/edgetunnel/main/AutoTest.txt"
                .to_string(),
            nat64_prefix: "2a02:898:146:64::".to_string(),
            doh_address: "1.1.1.1".to_string(),
            proxy_ip: "proxyip.cmliussss.net".to_string(),
            vless_split_1: "v2".to_string(),
            vless_split_2: "ray".to_string(),
            clash_split_1: "cla".to_string(),
            clash_split_2: "sh".to_string(),
        }
    }
}

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let mut config = Config::default();

    // 从环境变量中获取配置
    if let Some(sub_path) = env.var("SUB_PATH").ok() {
        config.sub_path = sub_path.to_string();
    }
    config.uuid = generate_uuid(&config.sub_path);
    if let Some(txt_url) = env.var("TXT_URL").ok() {
        config.txt_url = txt_url.to_string();
    }
    if let Some(nat64) = env.var("NAT64").ok() {
        config.nat64_prefix = nat64.to_string();
    }
    if let Some(doh) = env.var("DOH").ok() {
        config.doh_address = doh.to_string();
    }
    if let Some(proxy_ip) = env.var("PROXY_IP").ok() {
        config.proxy_ip = proxy_ip.to_string();
    }

    let url = req.url()?;
    let upgrade_header = req.headers().get("Upgrade");
    let is_websocket = if let Ok(Some(h)) = upgrade_header {
        h.to_lowercase() == "websocket"
    } else {
        false
    };
    let not_websocket = !is_websocket;

    let vless_path = format!(
        "/{}/{}{}",
        urlencoding::encode(&config.sub_path),
        config.vless_split_1,
        config.vless_split_2
    );
    let clash_path = format!(
        "/{}/{}{}",
        urlencoding::encode(&config.sub_path),
        config.clash_split_1,
        config.clash_split_2
    );
    let proxy_prefix = format!("/{}/", urlencoding::encode(&config.sub_path));

    if not_websocket {
        if url.path() == vless_path {
            // 获取优选列表
            let preferred_list: Vec<String> = get_preferred_list(&config.txt_url).await?;
            let host: String = req.headers().get("Host")?.unwrap_or_default();
            return vless_config_file(&config, &preferred_list, &host);
        } else if url.path() == clash_path {
            let preferred_list = get_preferred_list(&config.txt_url).await?;
            let host = req.headers().get("Host")?.unwrap_or_default();
            return clash_config_file(&config, &preferred_list, &host);
        } else if url.path() == format!("/{}", urlencoding::encode(&config.sub_path)) {
            let user_agent = req
                .headers()
                .get("User-Agent")?
                .unwrap_or_default()
                .to_lowercase();
            let preferred_list = get_preferred_list(&config.txt_url).await?;
            let host = req.headers().get("Host")?.unwrap_or_default();

            return if user_agent
                .contains(&format!("{}{}", config.vless_split_1, config.vless_split_2))
            {
                vless_config_file(&config, &preferred_list, &host)
            } else if user_agent
                .contains(&format!("{}{}", config.clash_split_1, config.clash_split_2))
            {
                clash_config_file(&config, &preferred_list, &host)
            } else {
                tips_page(&config).await
            };
        }
    }

    // 代理无法访问CF CDN
    if url.path().starts_with(&proxy_prefix) && url.path() != vless_path && url.path() != clash_path
    {
        let target = url.path()[proxy_prefix.len()..].to_string();
        let decoded_target = urlencoding::decode(&target)
            .map(|v| v.to_string())
            .unwrap_or(target);

        let new_url = &format!("{}{}", decoded_target, url.query().unwrap_or_default());
        let proxy_req = Request::new_with_init(
            new_url,
            &RequestInit {
                method: req.method(),
                headers: req.headers().clone(),
                ..Default::default()
            }
            .with_body(req.clone_mut()?.bytes().await.ok().map(Into::into)),
        )?;

        match Fetch::Request(proxy_req).send().await {
            Ok(response) => Ok(response),
            Err(_) => Response::error("Not Found", 404),
        }
    } else if not_websocket {
        Response::error("Not Found", 404)
    } else if is_websocket {
        handle_upgrade_websocket(req, config).await
    } else {
        Response::error("Bad Request", 400)
    }
}

async fn handle_upgrade_websocket(req: Request, config: Config) -> Result<Response> {
    let websocket_pair = WebSocketPair::new()?;
    let server = websocket_pair.server;
    let client = websocket_pair.client;

    if let Some(sec_websocket_protocol) = req.headers().get("sec-websocket-protocol")? {
        let decoded_data = base64_decode(&sec_websocket_protocol)?;
        parse_vless_header(decoded_data, server, &config).await?;
    }

    Ok(Response::empty()?
        .with_status(101)
        .with_websocket(Some(client)))
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let replaced = input.replace("-", "+").replace("_", "/");
    general_purpose::STANDARD
        .decode(replaced)
        .map_err(|e| Error::RustError(format!("Base64 decode error: {}", e)))
}

async fn parse_vless_header(data: Vec<u8>, ws: WebSocket, config: &Config) -> Result<()> {
    if data.len() < 18 {
        return Err(Error::RustError("Invalid VLESS header".to_string()));
    }

    let uuid_bytes = &data[1..17];
    if verify_vless_key(uuid_bytes) != config.uuid {
        return Err(Error::RustError("Invalid UUID".to_string()));
    }

    let address_type_index = data[17] as usize;
    let port_index = 18 + address_type_index + 1;

    if port_index + 2 > data.len() {
        return Err(Error::RustError("Invalid port index".to_string()));
    }

    let port_bytes = &data[port_index..port_index + 2];
    let port = u16::from_be_bytes(port_bytes.try_into().unwrap());

    let address_type_index = port_index + 2;
    if address_type_index >= data.len() {
        return Err(Error::RustError("Invalid address type index".to_string()));
    }

    let address_type = data[address_type_index];
    let mut address_length = 0;
    let mut address = String::new();
    let mut address_data_index = address_type_index + 1;

    match address_type {
        1 => {
            // IPv4
            address_length = 4;
            if address_data_index + address_length > data.len() {
                return Err(Error::RustError("Invalid IPv4 address".to_string()));
            }
            let ipv4_bytes = &data[address_data_index..address_data_index + address_length];
            address = ipv4_bytes
                .iter()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join(".");
        }
        2 => {
            // Domain
            if address_data_index >= data.len() {
                return Err(Error::RustError("Invalid domain length".to_string()));
            }
            address_length = data[address_data_index] as usize;
            address_data_index += 1;
            if address_data_index + address_length > data.len() {
                return Err(Error::RustError("Invalid domain address".to_string()));
            }
            address =
                str::from_utf8(&data[address_data_index..address_data_index + address_length])?
                    .to_string();
        }
        3 => {
            // IPv6
            address_length = 16;
            if address_data_index + address_length > data.len() {
                return Err(Error::RustError("Invalid IPv6 address".to_string()));
            }
            let ipv6_bytes = &data[address_data_index..address_data_index + address_length];
            let mut ipv6_parts = Vec::new();
            for i in 0..8 {
                let part = u16::from_be_bytes(ipv6_bytes[i * 2..i * 2 + 2].try_into().unwrap());
                ipv6_parts.push(format!("{:x}", part));
            }
            address = ipv6_parts.join(":");
        }
        _ => {
            return Err(Error::RustError("Unknown address type".to_string()));
        }
    }

    let initial_data = &data[address_data_index + address_length..];

    // 尝试直接连接
    let socket = match connect_socket(&address, port, config).await {
        Ok(socket) => socket,
        Err(e) => return Err(e),
    };

    establish_tunnel(ws, socket, initial_data).await
}



async fn connect_socket(address: &str, port: u16, config: &Config) -> Result<Socket> {
    // 尝试直接连接
    if let Ok(socket) = Socket::builder().connect(address, port) {
        return Ok(socket);
    }
    // 直接连接失败，尝试NAT64
    if !config.nat64_prefix.is_empty() {
        let nat64_address = match address.parse::<std::net::Ipv4Addr>() {
            Ok(ipv4) => convert_ipv4_to_nat64(&ipv4, &config.nat64_prefix),
            Err(_) => {
                // 解析域名到IPv4
                match resolve_domain_to_ipv4(address, &config.doh_address).await {
                    Ok(ipv4) => convert_ipv4_to_nat64(&ipv4, &config.nat64_prefix),
                    Err(_) => return Err(Error::RustError("Failed to resolve domain".to_string())),
                }
            }
        };

        match Socket::builder().connect(&nat64_address, port) {
            Ok(socket) => Ok(socket),
            Err(_) => {
                // NAT64连接失败，尝试代理
                if !config.proxy_ip.is_empty() {
                    let proxy_parts: Vec<&str> = config.proxy_ip.split(':').collect();
                    let proxy_host = proxy_parts[0];
                    let proxy_port = proxy_parts
                        .get(1)
                        .unwrap_or(&"443")
                        .parse::<u16>()
                        .unwrap_or(443);

                    match Socket::builder().connect(proxy_host, proxy_port) {
                        Ok(socket) => Ok(socket),
                        Err(_) => {
                            Err(Error::RustError(
                                "All connection methods failed".to_string(),
                            ))
                        }
                    }
                } else {
                    Err(Error::RustError(
                        "NAT64 connection failed and no proxy configured".to_string(),
                    ))
                }
            }
        }
    } else if !config.proxy_ip.is_empty() {
        // 没有NAT64前缀，尝试代理
        let proxy_parts: Vec<&str> = config.proxy_ip.split(':').collect();
        let proxy_host = proxy_parts[0];
        let proxy_port = proxy_parts
            .get(1)
            .unwrap_or(&"443")
            .parse::<u16>()
            .unwrap_or(443);

        match Socket::builder().connect(proxy_host, proxy_port) {
            Ok(socket) => Ok(socket),
            Err(_) => {
                Err(Error::RustError(
                    "Direct and proxy connection failed".to_string(),
                ))
            }
        }
    } else {
        return Err(Error::RustError(
            "Direct connection failed and no fallback configured".to_string(),
        ));
    }
}

fn convert_ipv4_to_nat64(ipv4: &std::net::Ipv4Addr, nat64_prefix: &str) -> String {
    let clean_prefix = nat64_prefix.split('/').next().unwrap_or(nat64_prefix);
    let octets = ipv4.octets();
    format!(
        "[{}{:02x}{:02x}:{:02x}{:02x}]",
        clean_prefix, octets[0], octets[1], octets[2], octets[3]
    )
}

async fn resolve_domain_to_ipv4(domain: &str, doh_address: &str) -> Result<std::net::Ipv4Addr> {
    let url = format!("https://{}/dns-query?name={}&type=A", doh_address, domain);
    let req = Request::new_with_init(
        &url,
        &RequestInit {
            method: Method::Get,
            headers: {
                let mut headers = Headers::new();
                headers.set("Accept", "application/dns-json")?;
                headers
            },
            ..Default::default()
        },
    )?;

    let mut resp = Fetch::Request(req).send().await?;
    let json: serde_json::Value = resp.json().await?;

    if let Some(answers) = json.get("Answer").and_then(|a| a.as_array()) {
        for answer in answers {
            if answer.get("type").and_then(|t| t.as_u64()) == Some(1) {
                if let Some(data) = answer.get("data").and_then(|d| d.as_str()) {
                    return data
                        .parse()
                        .map_err(|_| Error::RustError("Failed to parse IPv4 address".to_string()));
                }
            }
        }
    }

    Err(Error::RustError("No A record found".to_string()))
}

fn verify_vless_key(arr: &[u8]) -> String {
    let key_format: Vec<String> = (0..256).map(|i| format!("{:02x}", i)).collect();

    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        key_format[arr[0] as usize],
        key_format[arr[1] as usize],
        key_format[arr[2] as usize],
        key_format[arr[3] as usize],
        key_format[arr[4] as usize],
        key_format[arr[5] as usize],
        key_format[arr[6] as usize],
        key_format[arr[7] as usize],
        key_format[arr[8] as usize],
        key_format[arr[9] as usize],
        key_format[arr[10] as usize],
        key_format[arr[11] as usize],
        key_format[arr[12] as usize],
        key_format[arr[13] as usize],
        key_format[arr[14] as usize],
        key_format[arr[15] as usize],
    )
    .to_lowercase()
}

async fn establish_tunnel(ws: WebSocket, mut socket: Socket, initial_data: &[u8]) -> Result<()> {
    ws.accept()?;
    ws.send_with_bytes(&[0, 0])?;
    //
    // let mut writer = socket.writable()?;
    // let mut reader = socket.readable()?;

    use std::task::{ContextBuilder, Waker, Poll};
    use std::future::Future;

    let waker = Waker::noop();

    let mut cx = ContextBuilder::from_waker(&waker)
        .build();

    let _ = socket.write(b"");
    if !initial_data.is_empty() {
        socket.poll_write(initial_data).await?;
    }

    // WebSocket -> Socket
    let ws_clone = ws.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let mut stream = ws_clone.into_stream();
        while let Some(Ok(Message::Bytes(data))) = stream.next().await {
            if let Err(_) = writer.write_all(&data).await {
                break;
            }
        }
    });

    // Socket -> WebSocket
    wasm_bindgen_futures::spawn_local(async move {
        let mut buffer = vec![0; 65536];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(_) = ws.send_with_bytes(&buffer[..n]) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(())
}

fn generate_uuid(sub_path: &str) -> String {
    let encoded = sub_path
        .as_bytes()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let twenty_chars = format!("{:0<20}", &encoded[..encoded.len().min(20)]);
    let first_eight = &twenty_chars[..8];
    let last_twelve = &twenty_chars[8..];

    format!("{}-0000-4000-8000-{}", first_eight, last_twelve)
}

async fn get_preferred_list(txt_url: &str) -> Result<Vec<String>> {
    if txt_url.is_empty() {
        return Ok(Vec::new());
    }

    let req = Request::new_with_init(txt_url, &RequestInit::default())?;
    let mut resp = Fetch::Request(req).send().await?;
    let text = resp.text().await?;

    Ok(text
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

fn process_preferred_list(preferred_list: &[String], host_name: &str) -> Vec<NodeInfo> {
    let mut result = vec![NodeInfo {
        address: host_name.to_string(),
        port: 443,
        name: "原生节点".to_string(),
    }];

    for (i, item) in preferred_list.iter().enumerate() {
        let parts: Vec<&str> = item.split('#').collect();
        let address_port = parts[0];
        let node = format!("节点 {}", i + 1);
        let name = parts.get(1).map(|v| v.to_string()).unwrap_or(node);

        let address_parts: Vec<&str> = address_port.split(':').collect();
        let port = if address_parts.len() > 1 {
            address_parts
                .last()
                .unwrap_or(&"443")
                .parse()
                .unwrap_or(443)
        } else {
            443
        };
        let address = address_parts[..address_parts.len().saturating_sub(1)].join(":");

        result.push(NodeInfo {
            address,
            port,
            name: name.to_string(),
        });
    }

    result
}

struct NodeInfo {
    address: String,
    port: u16,
    name: String,
}

async fn tips_page(config: &Config) -> Result<Response> {
    let html = format!(
        "<title>订阅-{}</title>
        <style>
            body {{
                font-size: 25px;
                text-align: center;
            }}
        </style>
        <strong>请把链接导入 {}{} 或 {}{}</strong>",
        config.sub_path,
        config.clash_split_1,
        config.clash_split_2,
        config.vless_split_1,
        config.vless_split_2
    );

    Response::from_html(html)
}

fn vless_config_file(
    config: &Config,
    preferred_list: &[String],
    host_name: &str,
) -> Result<Response> {
    let nodes = process_preferred_list(preferred_list, host_name);
    let content = nodes.iter()
        .map(|node| {
            format!(
                "vless://{}@{}:{}?encryption=none&security=tls&sni={}&fp=chrome&type=ws&host={}&path={}#{}",
                config.uuid,
                node.address,
                node.port,
                host_name,
                host_name,
                urlencoding::encode("/?ed=2560"),
                urlencoding::encode(&node.name)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let headers = Headers::default();
    headers.append("Content-Type", "text/plain;charset=utf-8")?;
    Ok(Response::ok(content)?.with_headers(headers))
}

fn clash_config_file(
    config: &Config,
    preferred_list: &[String],
    host_name: &str,
) -> Result<Response> {
    let nodes = process_preferred_list(preferred_list, host_name);

    let node_configs = nodes
        .iter()
        .map(|node| {
            format!(
                "- name: {}
  type: vless
  server: {}
  port: {}
  uuid: {}
  udp: true
  tls: true
  sni: {}
  network: ws
  ws-opts:
    path: \"/?ed=2560\"
    headers:
      Host: {}
      User-Agent: Chrome",
                node.name, node.address, node.port, config.uuid, host_name, host_name
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let proxy_names = nodes
        .iter()
        .map(|node| format!("    - {}", node.name))
        .collect::<Vec<_>>()
        .join("\n");

    let content = format!(
        "proxies:
{}
proxy-groups:
- name: 海外规则
  type: select
  proxies:
    - 延迟优选
    - 故障转移
    - DIRECT
    - REJECT
{}
- name: 国内规则
  type: select
  proxies:
    - DIRECT
    - 延迟优选
    - 故障转移
    - REJECT
{}
- name: 广告屏蔽
  type: select
  proxies:
    - REJECT
    - DIRECT
    - 延迟优选
    - 故障转移
{}
- name: 延迟优选
  type: url-test
  url: https://www.google.com/generate_204   
  interval: 30
  tolerance: 50
  proxies:
{}
- name: 故障转移
  type: fallback
  url: https://www.google.com/generate_204   
  interval: 30
  proxies:
{}
rules:
  - GEOSITE,category-ads-all,广告屏蔽
  - GEOSITE,cn,国内规则
  - GEOIP,CN,国内规则,no-resolve
  - MATCH,海外规则",
        node_configs, proxy_names, proxy_names, proxy_names, proxy_names, proxy_names
    );

    let headers = Headers::default();
    headers.append("Content-Type", "text/plain;charset=utf-8")?;
    Ok(Response::ok(content)?.with_headers(headers))
}
