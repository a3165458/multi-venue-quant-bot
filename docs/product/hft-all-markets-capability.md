# Lighter 全市场高频量化能力契约

状态：Phase 0 进行中
日期：2026-07-29

已确认的产品决策：

- 用户目标中的“300 次/秒”原指实际成交；
- 当前账户为 Standard；
- 由于实际成交不可由客户端保证，且 Standard 仅允许约 60 个请求/分钟，第一阶段正式重定义为全市场扫描、300+ 行情事件/秒、只读观测；订单执行仍保持现有低频路径。

## CAPABILITY

系统面向自营量化交易操作者，持续发现并订阅 Lighter 可交易市场，维护可验证的实时盘口，在风险与交易所配额内对市场机会进行评分，并通过低延迟订单通道执行做市、跨市场价差或统计套利策略。

第一阶段的性能承诺是：

- 持续处理不少于 300 条市场数据事件/秒；
- 订单动作吞吐受账户实时配额、volume quota 和风险预算硬限制；
- 不承诺每秒 300 次实际成交，也不承诺盈利；
- 监控全部合格市场，但只在通过流动性、价差、延迟和风险筛选的市场交易。

## CONSTRAINTS

### 交易所硬约束

- Standard 账户 REST/交易请求上限为 60 次/分钟。
- Premium 账户的 `sendTx` / `sendTxBatch` 上限由质押 LIT 决定：
  - 0 LIT：4,000 次/分钟，约 66.7 次/秒；
  - 100,000 LIT：12,000 次/分钟，约 200 次/秒；
  - 300,000 LIT：24,000 次/分钟，约 400 次/秒；
  - 500,000 LIT：40,000 次/分钟，约 666.7 次/秒。
- 300 个交易请求/秒等于 18,000 次/分钟；仅从公布的速率档位看，至少需要 Premium + 300,000 LIT 档位。
- 批量请求不会减少 volume quota 消耗：批次中的每个交易仍分别计入 quota。
- WebSocket 每个 IP 最多 100 个连接、每连接 100 个订阅、总计 1,000 个订阅、50 个在途客户端消息。
- 每个 API Key 有独立 nonce；nonce 必须严格递增。不能在错误后由多个并发任务各自刷新并覆盖本地 nonce。
- 服务端接受 `sendTx` 只代表请求语法被接受，不代表订单成交或最终被 sequencer 接受；最终状态必须来自账户订单/成交 WebSocket。
- `order_book` 频道约每 50ms 批量推送更新。必须校验当前 `begin_nonce` 与上一更新的 `nonce`，断档后重新订阅并重建快照。

官方依据：

- https://apidocs.lighter.xyz/docs/rate-limits
- https://apidocs.lighter.xyz/docs/volume-quota-program
- https://apidocs.lighter.xyz/docs/account-types
- https://apidocs.lighter.xyz/docs/websocket-reference
- https://apidocs.lighter.xyz/docs/trading
- https://apidocs.lighter.xyz/docs/api-keys

### 系统不变量

- 风控决策优先于策略和吞吐目标；kill switch 不得依赖 Dashboard 可用性。
- 一个 API Key 只能有一个 nonce 分配器和一个有序提交队列。
- 本地订单状态必须区分：意图、已签名、已提交、API 接受、sequencer 接受、挂单、部分成交、完全成交、撤销、拒绝和状态未知。
- 不允许根据 REST 200 响应直接增加已成交仓位。
- 盘口断档、账户流断档、nonce 不确定、时钟漂移超限或风险状态未知时，对应市场进入 `HALTED`。
- 队列必须有界。积压时合并或丢弃过期行情/报价意图，不允许无限积压订单动作。
- 策略不得通过多地址或多账户规避交易所限制。
- 禁止自成交、刷量、虚假挂单或任何操纵市场的行为。

### 当前仓库差距

- 当前 WebSocket 使用单连接并为每个市场订阅 `order_book`、`trade`、`ticker` 三个频道，没有按每连接 100 个订阅自动分片。
- `broadcast(4096)` 只传播解析后的对象；交易批次只保留最后一笔交易。
- 盘口解析没有保存 `nonce` / `begin_nonce`，无法检测断档，也没有增量盘口状态机。
- 策略、风控、REST 下单和 Dashboard 主要集中在一个约 2,100 行的 `main.rs` 中。
- 下单只使用 REST `sendTx`，没有交易 WebSocket、`sendTxBatch`、改单或每 API Key 的有序执行 actor。
- 任一订单失败后会刷新共享 nonce；在高并发下可能与已分配但尚未完成的 nonce 冲突。
- 账户和挂单状态主要通过周期性 REST 刷新，不适合作为高频执行的真实状态源。
- 热路径包含大量 `info!` 日志、JSON 动态值、全量克隆和锁竞争。
- 当前回测以 K 线为主，无法评估队列位置、部分成交、盘口冲击和微秒/毫秒级延迟。

## IMPLEMENTATION CONTRACT

### Actors

- Market Discovery：发现市场并维护交易规格、状态和可交易性。
- Feed Supervisor：建立和重连 WebSocket 分片。
- Book Builder：按市场维护盘口快照、增量和连续性。
- Opportunity Engine：计算价差、微价格、订单流不平衡、波动和跨市场偏离。
- Strategy Workers：只输出带有效期的订单意图，不直接访问网络。
- Portfolio Risk：统一持仓、保证金、市场/账户敞口和熔断。
- Order Coordinator：净额化、去重、节流并选择 API Key。
- Nonce/Execution Actor：每个 API Key 串行分配 nonce、签名和提交。
- Account State Reconciler：消费账户订单、成交和仓位流，处理未知状态。
- Recorder/Replay：记录原始市场与账户事件，支持确定性重放。
- Operator：通过仅本地或强鉴权控制面观察、降速、暂停和紧急撤单。

### Surfaces

```text
Market discovery
      |
      v
WS shards -> bounded raw-event queues -> per-market book actors
                                          |
                                          v
                              opportunity/strategy workers
                                          |
                                          v
                                 portfolio risk gate
                                          |
                                          v
                              intent netting + rate limiter
                                          |
                                          v
                          per-key nonce/execution actors
                                          |
                       REST/WS sendTx or sendTxBatch
                                          |
                                          v
                         authenticated account streams
                                          |
                                          v
                            state reconciliation + PnL
```

### Required states and transitions

市场状态：

```text
DISCOVERED -> SYNCING -> LIVE -> STALE
                    \-> HALTED
STALE/HALTED -> SYNCING only after fresh snapshot and continuity validation
```

订单状态：

```text
INTENT -> RISK_APPROVED -> SIGNED -> SUBMITTED -> API_ACCEPTED
                                            \-> REJECTED
API_ACCEPTED -> OPEN -> PARTIAL -> FILLED
                    \-> CANCEL_PENDING -> CANCELED
                    \-> UNKNOWN -> RECONCILED
```

### Interface and data implications

- `MarketEvent` 必须保留服务器时间、接收时间、market_id、channel、offset、nonce、begin_nonce 和原始批次大小。
- `OrderIntent` 必须包含 strategy_id、market_id、side、price、quantity、post_only、reduce_only、TTL、优先级和幂等 client_order_id。
- `RateBudget` 必须按账户等级、质押档位、滚动分钟、volume quota 和本地安全余量计算。
- `RiskSnapshot` 必须包含权益、可用保证金、净/总敞口、每市场敞口、未确认订单风险和数据新鲜度。
- 热路径不使用 `serde_json::Value` 作为内部数据模型。
- 所有跨 actor 队列均需要容量、丢弃策略、队列延迟和高水位指标。

### Performance service levels

第一阶段：

- 300+ 行情事件/秒持续处理；
- 事件队列 p99 排队延迟小于 5ms；
- 单市场策略决策 p99 小于 1ms（不含网络和签名）；
- 订单意图到提交 p99 必须先测量再设承诺；
- 零盘口静默断档；
- 速率限制 429 不作为正常控制机制。

300+ 订单动作/秒属于后续独立验收项，只有账户官方配额、资金规模、volume quota、网络 RTT、签名耗时和风险预算全部满足后才能启用。

### Observability and operations

- 指标：每频道事件率、断档次数、盘口年龄、策略延迟、签名延迟、提交 RTT、429、拒单、未知订单、成交率、maker/taker 比例、滑点和 adverse selection。
- 结构化审计：每个订单从策略输入到最终成交/撤销的完整因果链。
- 分级熔断：单市场、单策略、单 API Key、全账户。
- 启动默认为观测或 shadow 模式；实盘需要显式开关和名义金额上限。

## DELIVERY PHASES

### Phase 0：测量与安全基线

- 修复 Dashboard 鉴权、监听地址和危险端点。
- 增加事件吞吐、队列、策略和下单 RTT 基准。
- 建立 raw event recorder 和离线 replay。
- 验收：不下单时稳定处理 300+ 事件/秒，连续性缺口可检测且可恢复。

当前证据（2026-07-29 主网只读烟雾测试）：

- 自动发现 219 个永续市场并分为 3 条 ticker-only WebSocket 连接；
- 启动预热处理 33,255 条 BBO 事件；
- 正式 5 秒窗口处理 4,632 条事件，约 926 events/s；
- 169 个市场在窗口内产生有效双边 BBO；
- 修复预热消费后未再发生广播队列 lag；
- 测试不读取密钥、不加载签名库、不发送交易。

### Phase 1：全市场行情引擎

- 动态发现市场。
- 按订阅上限分片 WebSocket。
- 实现带 nonce 连续性检查的 per-market book actor。
- 使用 BBO/order book 作为主要信号，交易流作为补充，不重复订阅无用频道。

### Phase 2：单市场 maker shadow

- 只选择 1–3 个高流动市场。
- 实现库存感知双边报价、post-only、TTL 和 adverse-selection 保护。
- 只记录“本应下单”的意图，不提交交易。

### Phase 3：受限实盘

- 每个 API Key 单独 nonce/execution actor。
- 使用账户 WebSocket 对账。
- 极小名义金额、低动作率、硬损失上限。
- 逐级提高市场数和动作率，任何阶段不得直接跳到 300/s。

### Phase 4：多市场与批处理

- 增加跨市场机会排序、全局资金分配和订单意图净额化。
- 根据实测评估交易 WebSocket 与 `sendTxBatch`。
- 只有在持续正向的扣费后、滑点后样本外结果下提高吞吐。

## NON-GOALS

- 保证盈利或保证成交数量。
- 第一版覆盖所有策略类型。
- 通过刷量、自成交、诱导性挂单或规避限制获得收益。
- 使用分钟/小时 K 线回测证明高频策略有效。
- 在未认证的公网 Dashboard 上控制实盘。
- 为追求吞吐而绕过风险、连续性或订单状态确认。

## OPEN QUESTIONS

以下决策会改变架构或是否能够达到 300 订单请求/秒：

1. 初始资金、单日最大可接受损失、单市场和全账户最大敞口是多少？
2. 只做主网永续，还是也包括 Robinhood Chain 和现货？
3. 优先策略是 maker 做市、跨市场套利，还是统计套利？
4. 机器人部署地区及到 Lighter API 的实测 p50/p99 RTT 是多少？
5. 是否允许创建多个 API Key，用不同 key 隔离 maker、cancel 和紧急操作？

## HANDOFF

当前状态需要架构与产品约束确认，不能直接进入“300 成交/秒”实现。

确认 OPEN QUESTIONS 后，按 Phase 0 开始：

1. 用 `security-review` 封闭控制面；
2. 用 `tdd-workflow` 实现事件模型、连续性检查和 bounded queue；
3. 用 `eval-harness` 建立吞吐、延迟、断档恢复和 shadow 策略验收；
4. 用 `verification-loop` 完成格式、Clippy、测试和回归验证。
