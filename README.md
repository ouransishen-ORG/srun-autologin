# srun-autologin

深澜（Srun）校园网自动登录工具箱：认证协议内核 + 跨平台 Web 面板。填好一次凭据，掉线自动接回；门户地址支持网络自动发现，任何深澜门户即插即用。

> 来源说明：本项目的认证协议内核基于 [zu1k/srun](https://github.com/zu1k/srun)（GPL-3.0），并参考了其下游校园适配实践；Web 面板 `srun-web` 为新增实现。项目最初在中国石油大学（北京）2026 年的校园网环境下开发与实测。

## 组成

- `srun` —— 原汁原味的深澜认证内核（CLI 登录、challenge/portal 协议、多用户配置），来自上游 zu1k/srun，本仓库仅做必要维护
- `srun-web` —— 跨平台 Web 面板（本仓库新增），浏览器打开即用，Linux / macOS / Windows 通用

## Web 面板：srun-web

```bash
cargo run --release --bin srun-web -- --config config.json
# 浏览器打开 http://127.0.0.1:8217
```

特性：

| | |
|---|---|
| **零配置起步** | 门户地址自动发现——未认证网络下探测 captive portal 拦截（HTTP 302 跳转取 Location）或 DNS 劫持（探测域名解析到内网 IP），发现即保存生效；https 门户保真原协议 |
| **门户设置** | 地址 / ac_id / type 面板内现填，保存落盘并热生效，免重启 |
| **凭据现填一次 → 落盘 → 沿用** | 勾选"记住凭据"写回配置文件（Unix 600 权限 / Windows 用户 ACL），之后登录框留空即用 |
| **自动重连守卫** | 30 秒巡检，掉线自动重新认证；手动注销后 10 分钟内不抢接；连续失败自动重新发现门户（换网络自适应） |
| **HTTPS 全链** | 默认启用 rustls（feature `tls`），优先 `https://` 连接门户 |
| **多端通用** | 无系统 GUI 依赖，常驻建议 systemd user 单元（Linux）/ 开机启动项（Windows / macOS），仅监听 `127.0.0.1` |

### 配置文件

```json
{
  "server": "https://login.example.edu.cn",
  "users": [{ "username": "学号/工号", "password": "..." }]
}
```

全部字段都可以不填——首次启动会尝试自动发现门户，凭据在面板里现填、勾选"记住凭据"后落盘。

## 从源码运行

```bash
cargo build --release          # 内核 CLI（srun）
cargo build --release --bin srun-web   # Web 面板
```

Linux 常驻（systemd user 单元）示例：

```ini
[Unit]
Description=Srun Auto-Login Web Panel
After=network-online.target

[Service]
ExecStart=%h/path/to/srun-autologin/target/release/srun-web --config %h/path/to/config.json
Restart=on-failure

[Install]
WantedBy=default.target
```

## 安全说明

- 凭据仅存本机：Unix 下配置文件锁 600；Windows 下落在用户 Profile，由 ACL 隔离
- 密码不落日志：错误输出已脱敏
- 面板仅监听 `127.0.0.1`，不对局域网开放
- srun 协议本身以 `HMAC-MD5(密码, challenge)` 摘要过网，非明文；面板默认 https 连接门户，消除被动窃听面

## 许可

GPL-3.0（见 [LICENSE](LICENSE)），随上游 zu1k/srun 的许可证传染条款而沿用。

## 作者

- **偶然死神**（[@ouransishen](https://github.com/ouransishen)）—— 项目发起与整体整合
- **ouransishen-agent**（[@ouransishen-agent](https://github.com/ouransishen-agent)）—— Web 面板 `srun-web` 实现（机器共同创作者，隶属 [ouransishen-ORG](https://github.com/ouransishen-ORG)）
- **zu1k**（[zu1k/srun](https://github.com/zu1k/srun)）—— 认证协议内核原作者

Co-authored-by: ouransishen-agent <336650464+ouransishen-agent@users.noreply.github.com>
