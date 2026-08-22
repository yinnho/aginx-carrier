# aginx-carrier

> 分身 OS — aginx Agent 互联网上托管数字分身的运行时。

```
你 ──agent://──▶ aginx 网关 ──▶ aginx-carrier ──▶ 你的分身们
```

- **aginx 是路**（agent:// 网络），本仓是**网站**：每个部署托管自己的分身，分身跟着设备走（手机开着=在线，关机=离线——离线是正常状态，像人一样）。
- **用别人的分身**：算力在他家、数据在你家——session 在用户侧，素材内存级用完即销毁，产出文件回流。
- **分身定义层**文件级可移植（dup 格式），DupHub 是黄页+镜像 CDN。

从 [OpenCarrier](https://github.com/yinnho/carrier) fork 改造：单操作者、私有化部署、aginx 原生接入。总纲见 [../docs/AGINX-CARRIER-VISION.md](../docs/AGINX-CARRIER-VISION.md)。

## 状态

Phase 0-7a 已完成（types/memory/runtime/kernel/lifecycle/clone 全部搬运 + 剥多租户
+ wechat-oa 剥离 + 数据目录 pivot `~/.aginx/carrier/` + clone_install 入网钩子 +
`aginx-carrier acp` stdio 桥 + iLink 通道与 `aginx-carrier start` 守护形态 +
carrier lib 化 + crates/uniffi 移动绑定）。
**agent:// 第一刀已闭环**：本地 aginx 网关按 `~/.aginx/agents/<clone>/aginx.toml`
拉起本桥，端到端真实 LLM 对话实测通过。

下一步：桌面 aginxium 集成 / 移动 Kotlin·Swift 壳 / relay 段联调。

## 使用

```bash
cargo build --workspace
aginx-carrier info      # 版本 + 数据目录（~/.aginx/carrier/）
aginx-carrier start     # 守护进程：kernel + iLink 通道 + cron（Ctrl-C 退出）
aginx-carrier acp --clone <name>   # stdio ACP 桥（被 aginx 网关拉起）
```
