// 校园网自动登录面板 —— 深澜(Srun)协议通用 Web 面板（GPL-3.0，随上游许可证）
//
// 复用本 crate 的 srun 内核库（而非 shell 调二进制），提供：
//   GET  /            内嵌单页 UI
//   GET  /api/status  在线状态（/cgi-bin/rad_user_info，200+CSV=在线，302/HTML=掉线）
//   POST /api/login   登录（空参=用配置文件账号，带参=覆盖）
//   POST /api/logout  注销（ip 缺省取当前会话 IP）
//
// 设计约束：零新增依赖（serde_json/ureq 已在依赖树），默认只绑 127.0.0.1。
use srun::{read_config_from_file, SrunClient, User};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// 配置文件落盘权限：Unix 锁 600（含明文密码）；Windows 用用户 Profile 的 ACL（默认仅本人可读），无需额外处理。
#[cfg(unix)]
fn lock_config_permissions(path: &str) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| format!("设置 600 权限失败: {e}"))
}

#[cfg(not(unix))]
fn lock_config_permissions(_path: &str) -> Result<(), String> {
    Ok(())
}
use std::sync::Arc;
use std::time::Duration;

struct Ctx {
    server: std::sync::Mutex<String>,
    config_path: String,
    /// 最近一次手动注销成功的时间；自动重连守卫在其后的冷却窗口内不抢接
    last_logout: std::sync::Mutex<Option<std::time::Instant>>,
}

impl Ctx {
    fn server(&self) -> String {
        self.server.lock().unwrap().clone()
    }
}

fn main() {
    let mut bind = "127.0.0.1:8217".to_string();
    let mut server: Option<String> = None;
    let mut config_path = "config.json".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => bind = args.next().unwrap_or(bind),
            "--server" => server = Some(args.next().unwrap_or_default()),
            "--config" => config_path = args.next().unwrap_or(config_path),
            other => {
                eprintln!("未知参数 {other}；用法: srun-web [--bind 127.0.0.1:8217] [--server URL] [--config PATH]");
                std::process::exit(2);
            }
        }
    }
    // 服务器地址优先级：--server > 配置文件 server 字段 > 网络自动发现（拦截/DNS 劫持探测）> 编译期 AUTH_SERVER_IP 兜底（https 优先）
    let server = server.unwrap_or_else(|| {
        read_config_from_file(&config_path)
            .ok()
            .and_then(|c| c.server)
            .or_else(|| {
                println!("[discover] 配置无 server，尝试网络自动发现…");
                discover_server().map(|(s, _)| s)
            })
            .unwrap_or_else(|| format!("https://{}", env!("AUTH_SERVER_IP")))
    });
    let ctx = Arc::new(Ctx { server: std::sync::Mutex::new(server), config_path, last_logout: std::sync::Mutex::new(None) });
    let listener = TcpListener::bind(&bind).unwrap_or_else(|e| {
        eprintln!("监听 {bind} 失败: {e}");
        std::process::exit(1);
    });
    spawn_auto_relogin(&ctx);
    println!("校园网自动登录面板 → http://{bind}（server={}，config={}）", ctx.server(), ctx.config_path);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let ctx = Arc::clone(&ctx);
        std::thread::spawn(move || handle(stream, &ctx));
    }
}

fn handle(mut stream: TcpStream, ctx: &Ctx) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_uppercase();
    let path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    break;
                }
                if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            Err(_) => return,
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok();
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let (status, ctype, payload) = route(&method, &path, &body, ctx);
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes()).ok();
    stream.write_all(payload.as_bytes()).ok();
}

fn route(method: &str, path: &str, body: &str, ctx: &Ctx) -> (String, String, String) {
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => ("200 OK".into(), "text/html".into(), INDEX_HTML.into()),
        ("GET", "/api/status") => ("200 OK".into(), "application/json".into(), status_json(&ctx.server()).0),
        ("GET", "/api/config") => (
            "200 OK".into(),
            "application/json".into(),
            serde_json::json!({
                "saved": has_saved_credentials(&ctx.config_path),
                "server": ctx.server(),
                "config": ctx.config_path,
            })
            .to_string(),
        ),
        ("POST", "/api/config/clear") => {
            let out = match clear_credentials(&ctx.config_path) {
                Ok(_) => serde_json::json!({ "ok": true, "saved": false }).to_string(),
                Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
            };
            ("200 OK".into(), "application/json".into(), out)
        }
        ("POST", "/api/config") => {
            let out = match save_settings(body, ctx) {
                Ok(server) => {
                    *ctx.server.lock().unwrap() = server.clone(); // 即时生效，免重启
                    println!("[config] server → {server}");
                    serde_json::json!({ "ok": true, "server": server }).to_string()
                }
                Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
            };
            ("200 OK".into(), "application/json".into(), out)
        }
        ("POST", "/api/config/discover") => {
            let out = match discover_server() {
                Some((server, acid)) => {
                    let save = serde_json::json!({ "server": server, "acid": acid });
                    match save_settings(&save.to_string(), ctx) {
                        Ok(_) => {
                            *ctx.server.lock().unwrap() = server.clone();
                            println!("[discover] 发现并保存门户: {server}（acid={:?}）", acid);
                            serde_json::json!({ "ok": true, "server": server, "acid": acid }).to_string()
                        }
                        Err(e) => serde_json::json!({ "ok": false, "error": format!("发现 {server} 但保存失败: {e}") }).to_string(),
                    }
                }
                None => serde_json::json!({ "ok": false, "error": "未发现门户：可能已在线（在线时无需发现），或本网络不用拦截/DNS 劫持方式" }).to_string(),
            };
            ("200 OK".into(), "application/json".into(), out)
        }
        ("POST", "/api/login") => {
            println!("[login] 请求 body={body}");
            let (json, _) = login_json(body, ctx);
            ("200 OK".into(), "application/json".into(), json)
        }
        ("POST", "/api/logout") => {
            println!("[logout] 请求 body={body}");
            let (json, _) = logout_json(body, ctx);
            ("200 OK".into(), "application/json".into(), json)
        }
        _ => ("404 Not Found".into(), "text/plain".into(), "not found".into()),
    }
}

// ---------- 状态 ----------

/// 深澜 rad_user_info：在线返回 CSV，掉线时网关 302 到门户页（HTML）。
fn check_online(server: &str) -> Option<Vec<String>> {
    let url = format!("{server}/cgi-bin/rad_user_info");
    let resp = ureq::get(&url).timeout(Duration::from_secs(5)).call().ok()?;
    if resp.status() != 200 {
        return None;
    }
    let text = resp.into_string().ok()?;
    if text.trim_start().starts_with('<') {
        return None; // 被重定向到门户页
    }
    let fields: Vec<String> = text.trim().split(',').map(|s| s.trim().to_string()).collect();
    // 服务端掉线时也会 200 返回错误文本（如 not_online_error）；真实 CSV 首字段是时间戳
    if fields.len() < 9 || fields[0].parse::<i64>().is_err() {
        return None;
    }
    Some(fields)
}

fn status_json(server: &str) -> (String, bool) {
    match check_online(server) {
        Some(f) => {
            // 经验字段（SrunCGIAuthIntfSvr V1.18 实测）：1=上线时间 8=在线IP 3/4=收发字节
            let login_at = f.get(1).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            let ip = f.get(8).cloned().unwrap_or_default();
            let dur = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
                .saturating_sub(login_at);
            let online = serde_json::json!({
                "online": true,
                "ip": ip,
                "login_at": login_at,
                "online_seconds": dur,
                "raw": f,
            });
            (online.to_string(), true)
        }
        None => (r#"{"online":false}"#.to_string(), false),
    }
}

// ---------- 登录 / 注销 ----------

fn body_or_config_user(body: &str, ctx: &Ctx) -> Result<(Vec<User>, srun::Config, bool), String> {
    let parsed = serde_json::from_str::<serde_json::Value>(body).unwrap_or_default();
    let override_creds = {
        let u = parsed.get("username").and_then(|x| x.as_str()).map(String::from);
        let p = parsed.get("password").and_then(|x| x.as_str()).map(String::from);
        match (u, p) {
            (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => Some((u, p)),
            _ => None,
        }
    };
    let remember = parsed.get("remember").and_then(|v| v.as_bool()).unwrap_or(false);
    let config = read_config_from_file(&ctx.config_path).map_err(|e| format!("读取配置失败: {e}"))?;

    if let (Some((u, p)), true) = (&override_creds, remember) {
        if let Err(e) = save_credentials(&ctx.config_path, u, p) {
            eprintln!("[login] 凭据落盘失败: {e}"); // 不阻断登录，只提示
        }
    }

    let users: Vec<User> = match override_creds {
        Some((u, p)) => vec![User { username: u, password: p, ip: Some(String::new()), if_name: None }],
        None => config.clone().collect(),
    };
    if users.is_empty() {
        return Err("配置文件里没有账号，且请求未提供覆盖凭据".into());
    }
    Ok((
        users
            .into_iter()
            .map(|mut user| {
                if user.ip.is_none() && user.if_name.is_none() {
                    user.ip = Some(String::new()); // 触发 detect_ip
                }
                user
            })
            .collect(),
        config,
        remember,
    ))
}

// ---------- 凭据落盘（写回配置文件的 users 数组）----------

/// 把凭据写回配置文件 users[0]，保持其余字段不动（读原文 JSON 增量改，避免序列化丢字段）。
/// 600 权限：配置含明文密码，与仓库自带配置的安全约定一致。
fn save_credentials(config_path: &str, username: &str, password: &str) -> Result<(), String> {
    let raw = fs::read_to_string(config_path).map_err(|e| format!("读配置失败: {e}"))?;
    let mut v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("配置不是合法 JSON: {e}"))?;
    let users = v
        .as_object_mut()
        .ok_or("配置根不是对象")?
        .entry("users")
        .or_insert_with(|| serde_json::json!([{}]));
    if !users.is_array() {
        *users = serde_json::json!([{}]);
    }
    let arr = users.as_array_mut().unwrap();
    if arr.is_empty() {
        arr.push(serde_json::json!({}));
    }
    let first = arr[0].as_object_mut().ok_or("users[0] 不是对象")?;
    first.insert("username".into(), serde_json::json!(username));
    first.insert("password".into(), serde_json::json!(password));
    let out = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    fs::write(config_path, out).map_err(|e| format!("写配置失败: {e}"))?;
    lock_config_permissions(config_path)?;
    Ok(())
}

/// 清除落盘凭据：users[0] 的 username/password 置回占位空值。
fn clear_credentials(config_path: &str) -> Result<(), String> {
    save_credentials(config_path, "", "")
}

/// 保存设置：服务器地址（必填，http(s)://host[:port]）+ 可选 acid/type，写回配置文件。
fn save_settings(body: &str, ctx: &Ctx) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("请求不是合法 JSON: {e}"))?;
    let server = v
        .get("server")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("服务器地址不能为空")?;
    // 宽松校验：http(s):// + 非空 host（域名或 IP，可带端口）
    let ok = server.starts_with("http://") || server.starts_with("https://");
    let host = server
        .split("://")
        .nth(1)
        .map(|rest| rest.split('/').next().unwrap_or(""))
        .unwrap_or("");
    if !ok || host.is_empty() {
        return Err(format!("地址格式不对：{server}（应为 http://门户地址[:端口]）"));
    }

    let raw = fs::read_to_string(&ctx.config_path).map_err(|e| format!("读配置失败: {e}"))?;
    let mut cfg: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("配置不是合法 JSON: {e}"))?;
    let obj = cfg.as_object_mut().ok_or("配置根不是对象")?;
    obj.insert("server".into(), serde_json::json!(server));
    if let Some(acid) = v.get("acid").and_then(|x| x.as_i64()) {
        obj.insert("acid".into(), serde_json::json!(acid));
    }
    if let Some(utype) = v.get("type").and_then(|x| x.as_i64()) {
        obj.insert("type".into(), serde_json::json!(utype));
    }
    let out = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    fs::write(&ctx.config_path, out).map_err(|e| format!("写配置失败: {e}"))?;
    lock_config_permissions(&ctx.config_path)?;
    Ok(server.to_string())
}

fn has_saved_credentials(config_path: &str) -> bool {
    read_config_from_file(config_path)
        .map(|c| c.into_iter().next().map(|u| !u.username.is_empty() && !u.password.is_empty()).unwrap_or(false))
        .unwrap_or(false)
}

/// 自动发现门户：不依赖任何配置，从网络本身问出深澜门户地址。
/// 原理：校园网未认证时，网关会拦截 http 请求——要么 302 踢到门户（Location 即答案），
/// 要么 DNS 劫持把任意域名解析到门户 IP（被劫持的探测 IP 即答案，端口试 80/443/8080/8081）。
/// 已在线时 http 探测会正常通到外站（无 302），此时返回 None（在线了也不需要发现）。
/// 发现的地址带 `?ac_id=` 时同时回传 ac_id（深澜登录参数）。
fn discover_server() -> Option<(String, Option<i64>)> {
    const PROBE_URL: &str = "http://captive.apple.com/hotspot-detect.html"; // 专门给 captive portal 检测用的轻量页
    let client = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .timeout(Duration::from_secs(8))
        .redirects(0)
        .build();

    // 通道一：HTTP 302 拦截 → Location 就是门户（可能带 ac_id 等参数）
    if let Ok(resp) = client.get(PROBE_URL).call() {
        let loc = resp.header("Location").map(String::from);
        if let Some(loc) = loc {
            return parse_portal_url(&loc);
        }
        // 没跳转 = 已在线（或网络直通），无需发现
        return None;
    }

    // 通道二：DNS 劫持 → 探测域名解析到了非公网 IP（RFC1918 / 运营商内网），那就是门户
    let hijacked = std::net::ToSocketAddrs::to_socket_addrs(&(PROBE_URL.strip_prefix("http://").unwrap(), 80))
        .ok()?
        .next()?;
    let ip = hijacked.ip();
    let is_private = match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        std::net::IpAddr::V6(_) => false,
    };
    if !is_private {
        return None;
    }
    for port in [443u16, 80, 8080, 8081] {
        // 443 先试 https；其余端口按 http 试（自签 https 证书不在系统信任链，握手会失败）
        let (candidate, use_tls) = if port == 443 {
            (format!("https://{}:{}", ip, port), true)
        } else {
            (format!("http://{}:{}", ip, port), false)
        };
        if use_tls {
            // https 门户（合法证书）直接可判；自签的会在下面 http 分支兜底
            if let Ok(resp) = client.get(&candidate).call() {
                let ctype = resp.header("Content-Type").unwrap_or("");
                if ctype.contains("text/html") {
                    let body = resp.into_string().unwrap_or_default();
                    let acid = body.find("ac_id=").and_then(|i| {
                        body[i + 6..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
                    });
                    return Some((candidate, acid));
                }
            }
        } else if let Ok(resp) = client.get(&candidate).call() {
            let ctype = resp.header("Content-Type").unwrap_or("");
            if ctype.contains("text/html") {
                let body = resp.into_string().unwrap_or_default();
                let acid = body.find("ac_id=").and_then(|i| {
                    body[i + 6..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
                });
                return Some((candidate, acid));
            }
        }
    }
    None
}

/// 从拦截 Location 里抠出门户根地址 + ac_id，保留原始协议（https 门户不被降级）。
/// 形如 http(s)://10.x.x.x[:port]/srun_portal_pc?ac_id=1&... 或 http(s)://login.xxx.edu.cn/index_1.html
fn parse_portal_url(loc: &str) -> Option<(String, Option<i64>)> {
    let (scheme, rest) = loc
        .strip_prefix("https://")
        .map(|r| ("https", r))
        .or_else(|| loc.strip_prefix("http://").map(|r| ("http", r)))?;
    let host = rest.split('/').next()?; // host[:port]
    if host.is_empty() {
        return None;
    }
    let base = format!("{scheme}://{host}");
    let acid = loc.split("ac_id=").nth(1).and_then(|s| {
        s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
    });
    Some((base, acid))
}

fn login_json(body: &str, ctx: &Ctx) -> (String, bool) {
    let (users, config, _remember) = match body_or_config_user(body, ctx) {
        Ok(v) => v,
        Err(e) => return (serde_json::json!({ "ok": false, "error": e }).to_string(), false),
    };
    match try_login_once(&users, &config, ctx) {
        Ok((out, online)) => (out, online),
        Err(e) => (serde_json::json!({ "ok": false, "error": e }).to_string(), false),
    }
}

/// 用给定账号走一次完整认证（challenge → portal → 状态复核）。
/// 成功返回 (响应 JSON, 是否在线)；失败返回 Err(错误摘要)。
fn try_login_once(
    users: &[User],
    config: &srun::Config,
    ctx: &Ctx,
) -> Result<(String, bool), String> {
    let user = users.first().ok_or("没有可用账号")?.clone();
    println!("[login] 尝试 user={}", user.username);
    let mut client = SrunClient::new_from_user(&ctx.server(), user)
        .set_detect_ip(true)
        .set_strict_bind(config.strict_bind)
        .set_double_stack(config.double_stack);
    client.set_acid(config.acid.unwrap_or(1));
    client.set_type(config.utype.unwrap_or(1));
    if let Some(ref os) = config.os {
        client.set_os(os);
    }
    if let Some(ref name) = config.name {
        client.set_name(name);
    }
    if let Some(n) = config.n {
        client.set_n(n);
    }
    if let Some(d) = config.retry_delay {
        client.set_retry_delay(d);
    }
    if let Some(t) = config.retry_times {
        client.set_retry_times(t);
    }
    // 内核语义：重试耗尽也返回 Ok，真实结果要看 PortalResponse
    let resp = client.login_detail().map_err(|e| format!("{e}"))?;
    if !resp.success() {
        let msg = resp.error_summary();
        return Err(if msg.is_empty() { "认证失败（服务端未返回错误详情）".into() } else { msg });
    }
    // access_token 非空但带 ip_already_online_error：IP 已在线、本次未做新认证
    let note = resp.error_summary();
    let (status, online) = status_json(&ctx.server());
    let mut out = serde_json::json!({ "ok": true, "status": serde_json::from_str::<serde_json::Value>(&status).unwrap_or_default() });
    if !note.is_empty() {
        out["note"] = serde_json::json!(note);
    }
    Ok((out.to_string(), online))
}

/// 自动重连守卫：已保存凭据 + 检测到掉线 + 非手动注销冷却期 → 自动重新认证。
fn spawn_auto_relogin(ctx: &Arc<Ctx>) {
    const CHECK_EVERY: Duration = Duration::from_secs(30);
    const COOLDOWN_AFTER_LOGOUT: Duration = Duration::from_secs(600);
    const REDISCOVER_AFTER_FAILS: u32 = 3; // 连续失败 N 次后怀疑门户地址变了，重新发现
    let ctx = Arc::clone(ctx);
    std::thread::spawn(move || {
        let mut fail_streak: u32 = 0;
        loop {
            std::thread::sleep(CHECK_EVERY);
            if !has_saved_credentials(&ctx.config_path) {
                continue;
            }
            let (_, online) = status_json(&ctx.server());
            if online {
                fail_streak = 0;
                continue;
            }
            // 手动注销后的冷却窗口内不抢接，把"离线"决定权留给用户
            if let Some(t) = *ctx.last_logout.lock().unwrap() {
                if t.elapsed() < COOLDOWN_AFTER_LOGOUT {
                    continue;
                }
            }
            // 连续多次重连失败：多半不是密码错（凭据没变过），是门户地址变了/换了网络——重新发现
            if fail_streak >= REDISCOVER_AFTER_FAILS {
                println!("[auto] 连续 {fail_streak} 次重连失败，尝试重新发现门户…");
                if let Some((server, _)) = discover_server() {
                    if server != ctx.server() {
                        println!("[auto] 门户地址变化: {} → {}", ctx.server(), server);
                        *ctx.server.lock().unwrap() = server.clone();
                        let _ = save_settings(&serde_json::json!({ "server": server }).to_string(), &ctx);
                    }
                }
                fail_streak = 0;
            }
            println!("[auto] 检测到掉线，尝试自动重连");
            let attempt = (|| -> Result<(), String> {
                let config = read_config_from_file(&ctx.config_path).map_err(|e| format!("{e}"))?;
                let users: Vec<User> = config.clone().collect();
                try_login_once(&users, &config, &ctx).map(|_| ())
            })();
            match attempt {
                Ok(()) => {
                    println!("[auto] 重连成功");
                    fail_streak = 0;
                }
                Err(e) => {
                    eprintln!("[auto] 重连失败: {e}");
                    fail_streak += 1;
                }
            }
        }
    });
}

fn logout_json(body: &str, ctx: &Ctx) -> (String, bool) {
    let parsed = serde_json::from_str::<serde_json::Value>(body).unwrap_or_default();
    let username = parsed
        .get("username")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            read_config_from_file(&ctx.config_path)
                .ok()
                .and_then(|c| c.clone().into_iter().next().map(|u| u.username))
        });
    let Some(username) = username else {
        return (serde_json::json!({ "ok": false, "error": "没有可用的用户名" }).to_string(), false);
    };
    let ip = parsed
        .get("ip")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| check_online(&ctx.server()).map(|f| f.get(8).cloned().unwrap_or_default()))
        .unwrap_or_default();
    if ip.is_empty() {
        return (serde_json::json!({ "ok": false, "error": "拿不到会话 IP（可能已掉线）" }).to_string(), false);
    }
    println!("[logout] user={username} ip={ip}");
    match SrunClient::new_for_logout(&ctx.server(), &username, &ip).logout() {
        Ok(_) => {
            // 内核 logout 不回传成败，以状态复核为准
            let (status_json, still_online) = status_json(&ctx.server());
            let out = if still_online {
                serde_json::json!({ "ok": false, "error": "注销请求已发出，但会话仍在线", "status": serde_json::from_str::<serde_json::Value>(&status_json).unwrap_or_default() })
            } else {
                *ctx.last_logout.lock().unwrap() = Some(std::time::Instant::now());
                serde_json::json!({ "ok": true, "status": {"online": false} })
            };
            (out.to_string(), still_online)
        }
        Err(e) => (serde_json::json!({ "ok": false, "error": format!("{e}") }).to_string(), false),
    }
}

// ---------- 内嵌前端 ----------

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>校园网自动登录</title>
<style>
:root{color-scheme:dark}
body{font-family:system-ui,sans-serif;background:#10141c;color:#e6e9ef;margin:0;display:grid;place-items:center;min-height:100vh}
.card{width:min(560px,92vw);background:#171d29;border:1px solid #2a3346;border-radius:14px;padding:26px}
h1{font-size:19px;margin:0 0 4px}
.sub{color:#8b96ab;font-size:13px;margin-bottom:18px}
.pill{display:inline-block;padding:5px 14px;border-radius:99px;font-size:14px;font-weight:600}
.on{background:#123b2a;color:#3ddc97}.off{background:#3b1a1a;color:#ff7a7a}
.meta{color:#8b96ab;font-size:12.5px;margin:10px 0 18px;line-height:1.7;word-break:break-all}
label{display:block;font-size:12.5px;color:#8b96ab;margin:10px 0 4px}
input{width:100%;box-sizing:border-box;background:#0d1119;border:1px solid #2a3346;color:#e6e9ef;border-radius:8px;padding:9px 11px;font-size:14px}
.row{display:flex;gap:10px;margin-top:18px}
button{flex:1;padding:11px 0;border:0;border-radius:9px;font-size:14.5px;font-weight:600;cursor:pointer}
#login{background:#2563eb;color:#fff}#logout{background:#2a3346;color:#e6e9ef}#refresh{background:#2a3346;color:#8b96ab}
#log{margin-top:16px;font-size:12.5px;color:#8b96ab;min-height:44px;white-space:pre-wrap;background:#0d1119;border:1px solid #2a3346;border-radius:8px;padding:9px 11px}
</style></head><body><div class="card">
<h1>校园网自动登录 <span class="pill" id="pill">…</span></h1>
<div class="sub">深澜（Srun）认证 · 填好一次，掉线自动接回</div>
<div class="meta" id="meta">加载中…</div>
<div class="meta" id="rememberHint" style="margin:-8px 0 14px"></div>
<label>用户名（留空 = 用已保存/配置文件账号）</label>
<input id="username" autocomplete="off" placeholder="用户名 / 学号 / 工号">
<label>密码</label>
<input id="password" type="password" autocomplete="off" placeholder="••••••••">
<div style="display:flex;align-items:center;gap:8px;margin-top:10px;font-size:13px;color:#8b96ab">
  <input type="checkbox" id="remember" style="width:auto"> <label for="remember" style="margin:0;cursor:pointer">记住凭据（明文存入 config.json，600 权限）</label>
</div>
<div class="row">
  <button id="login" onclick="act('login')">登录</button>
  <button id="logout" onclick="act('logout')">注销</button>
  <button id="refresh" onclick="refresh()">刷新</button>
</div>
<div class="row"><button id="forget" style="background:#2a3346;color:#b0777a" onclick="forget()">清除已保存凭据</button></div>
<details style="margin-top:16px"><summary style="cursor:pointer;color:#8b96ab;font-size:13px">门户设置（其他学校 / 换地址）</summary>
<div style="margin-top:10px">
  <label>门户地址（http://域名或IP[:端口]，保存后即时生效并落盘）</label>
  <input id="cfgServer" autocomplete="off" placeholder="https://login.example.edu.cn">
  <div style="display:flex;gap:10px;margin-top:8px">
    <div style="flex:1"><label>ac_id（深澜参数，一般 1）</label><input id="cfgAcid" type="number" autocomplete="off"></div>
    <div style="flex:1"><label>type（一般 1）</label><input id="cfgType" type="number" autocomplete="off"></div>
  </div>
  <div class="row" style="margin-top:12px">
    <button id="saveCfg" style="background:#1d4ed8;color:#fff" onclick="saveCfg()">保存设置</button>
    <button id="discover" style="background:#2a3346;color:#7fb2ff" onclick="discover()">自动发现门户</button>
    <button id="reloadCfg" style="background:#2a3346;color:#8b96ab" onclick="loadCfg()">重读</button>
  </div>
  <div class="meta" style="margin:8px 0 0">没填过地址？点"自动发现"：设备未认证时网关会把任意网页拦截跳转到门户，从跳转里就能读出地址（DNS 劫持型网络也能探出来），发现即保存并生效。已在线时发现不出（也用不着）。</div>
</div>
</details>
<div id="log"></div>
</div>
<script>
const $=id=>document.getElementById(id);
function fmtDur(s){const d=Math.floor(s/86400),h=Math.floor(s%86400/3600),m=Math.floor(s%3600/60);return (d?d+"天 ":"")+h+"小时 "+m+"分";}
async function refresh(){
  try{
    const s=await (await fetch("/api/status")).json();
    if(s.online){
      $("pill").textContent="在线";$("pill").className="pill on";
      $("meta").innerHTML="IP <b>"+s.ip+"</b> · 已在线 "+fmtDur(s.online_seconds)+" · 服务端 "+(s.raw[s.raw.length-1]||"");
    }else{$("pill").textContent="离线";$("pill").className="pill off";$("meta").textContent="当前未通过门户认证（或 rad_user_info 不可达）";}
    const c=await (await fetch("/api/config")).json();
    $("rememberHint").textContent=c.saved?"已记住凭据：掉线后会自动接回（留空用户名即用）":"未记住凭据：登录时现填，勾选“记住凭据”后掉线自动接回";
  }catch(e){$("pill").textContent="面板异常";$("pill").className="pill off";$("meta").textContent=String(e);}
}
async function act(kind){
  $("log").textContent=kind==="login"?"登录中…":"注销中…";
  try{
    const r=await fetch("/api/"+kind,{method:"POST",headers:{"Content-Type":"application/json"},
      body:JSON.stringify({username:$("username").value.trim(),password:$("password").value,remember:$("remember").checked})});
    const j=await r.json();
    if(j.ok){
      if(kind==="logout"){$("log").textContent="✔ 已注销（10 分钟内不会自动重连）";}
      else{$("log").textContent="✔ 成功"+(j.note?"（"+j.note+"）":"");}
      if($("remember").checked&&kind==="login"){$("username").value="";$("password").value="";$("remember").checked=false;}
    }else{
      $("log").textContent="✘ 失败："+(j.error||"未知错误");
    }
    refresh();
  }catch(e){$("log").textContent="请求异常："+e;}
}
async function forget(){
  const r=await (await fetch("/api/config/clear",{method:"POST"})).json();
  $("log").textContent=r.ok?"已清除落盘凭据":"清除失败："+(r.error||"");
  refresh();
}
async function loadCfg(){
  const c=await (await fetch("/api/config")).json();
  $("cfgServer").value=c.server||"";
}
async function saveCfg(){
  $("log").textContent="保存中…";
  const payload={server:$("cfgServer").value.trim()};
  if($("cfgAcid").value) payload.acid=Number($("cfgAcid").value);
  if($("cfgType").value) payload.type=Number($("cfgType").value);
  const r=await (await fetch("/api/config",{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(payload)})).json();
  $("log").textContent=r.ok?"✔ 已保存并生效："+r.server:"✘ 保存失败："+(r.error||"");
  refresh();
}
async function discover(){
  $("log").textContent="探测网络中的门户…（未认证网络秒出，已在线网络会返回'未发现'）";
  const r=await (await fetch("/api/config/discover",{method:"POST"})).json();
  if(r.ok){$("cfgServer").value=r.server;$("log").textContent="✔ 发现门户并已保存生效："+r.server+(r.acid?"（ac_id="+r.acid+"）":"");}
  else{$("log").textContent="✘ "+(r.error||"未发现");}
  refresh();
}
refresh();loadCfg();setInterval(refresh,10000);
</script></body></html>"#;
