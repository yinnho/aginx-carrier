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

Phase 0 骨架建设中。搬运顺序：types → memory → runtime → kernel/lifecycle → clone → acp 桥（agent:// 打通）→ iLink 通道 → 三形态。

## 使用

```bash
cargo build --workspace
aginx-carrier info      # 版本 + 数据目录（~/.aginx/carrier/）
```
