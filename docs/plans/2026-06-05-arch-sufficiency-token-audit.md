<!-- Produced 2026-06-05 via an 8-agent audit workflow (5 code-grounded audits -> synthesize -> adversarial verify -> revise). Companion to 2026-06-05-inter-agent-info-exchange-design.md. All load-bearing claims ground-truthed against the tree. -->

# eli 架构充分性 & token 效率评估

## 一句话结论

- **架构充分性: 🟡 黄(偏绿)** — hook 表面 + 已建好但未接线的 taskboard/injector 让 Phase 1/2 能以"新 plugin + 后台订阅任务"落地、**核心几乎零改动**;唯一真正逼改核心的是 Phase 3 的两个全局单例(单槽 `INBOUND_INJECTOR` + `OnceLock TASK_STORE`),且都 reversible。
- **token 效率: 🟡 黄** — prompt caching **是真发出去的**(经复核确认,不是只被 measured),前缀也工程化做了 cache-stable;但**对话历史从不打 cache 断点** + **全量 tool schema(~40 工具,~3.8-5K tokens 估算)每轮全发**,这两个是最大漏点。

---

## ⚠️ Review 修正(2026-06-05 · 四透镜对抗复核 + 二次 ground-truth)

> 本节是 §3 原排序之上的**权威修正**。原文(§1-§3)保留作推理记录,但凡与本节冲突,**以本节为准**。修正点均已对代码二次核实。

**核心裁决:`worker + subscriber + cap→enqueue` 三者从 P0 降为 evidence-gated Phase 2;其余 P0 保留。** 理由:async-subagent 路径(tools.rs:2654/2694/2745)**已经**端到端回流且用 git artifacts;dormant `TaskWorker` 是**第二个、更差**的执行器(无 worktree、不调 collect_artifacts、worker.rs:250 做 2000-byte 截断,正是 design §2c 要杀的 telephone-game)。激活它 = 在零 demonstrated 需求下造 durable-queue 消费端 + 在平行路径重新引入信息丢失,违反 design §7 自家 evidence-gate。**最省力的回流路径是复用已工作的 subagent monitor,不点亮 TaskWorker。**

**动手前必改的 6 处(否则照 P0 写代码会撞墙):**

| # | 问题(已核实) | 改法 | 改哪份 |
|---|---|---|---|
| 1 | **P0#1 action 文本不可构建** —— `TaskEvent::Completed{id,result}`(mod.rs:129)**不带 session_origin**,"carry session_origin 作 session_id" 读不到 | 加 `session_origin` 进事件(additive,只动 store.rs 两个 broadcast 点);或 subscriber 走 `store.get(id)` 取(处理 None/TOCTOU) | audit §3 P0#1 |
| 2 | **无 stale-claim reaper** —— `last_heartbeat` 只写(worker.rs:102/219)从不读,`claim_next` 只选 `state='todo'`(store.rs:598),崩溃即永久卡 Claimed/Running | spawn worker 前必加 reaper(超时 heartbeat → 回 Todo 或 Failed{worker_lost}+retry cap)。**Phase-1 必备,非 Phase 2** | 两份 |
| 3 | **Lagged 丢事件 = 静默丢回流** —— broadcast 容量 128(store.rs:61),结果落库但推送丢 | subscriber recv 只入 mpsc 不跑 pipeline;加启动时 + Lagged 时的 reconciliation sweep(`reflowed` 标志补发)。交付语义 = best-effort-with-catchup | audit §3 P0;cross-ref design §6.1#5 |
| 4 | **injector 守卫应 warn-and-overwrite,不是 refuse** —— refuse 可能锁死早注册的占位 injector、静默搞挂 inbound,比今天的 last-write-wins 更糟 | 落 warn-and-overwrite(覆盖 `Some` 时 loud warn);refuse 留给 Phase-3 source-keyed registry | audit §3 P0#3 |
| 5 | **cap→enqueue + 保留 async 路径 = double-execution** —— 同一工作既被 worker claim 又被 model 重试 direct spawn,`NewTask` 无 idempotency key | enqueue 工作定 dedup key(`hash(objective+session_origin)`);或 enqueue 与 direct-spawn 互斥 | design §4.4 + audit P1 |
| 6 | **Phase-1 worker 产物与 design 自相矛盾** —— worker.rs:250 只产 2000-byte tail,而 design §2c/§4.3 要 git diff 串为主产物 | 要么显式标注 worker 为 "evidence-only,已知截断";要么上线前升到 subagent 标准(worktree+collect_artifacts) | 两份 |

**校准(方向不变):** history cache 断点收益**被高估但仍是 #1** —— 收益受 §"Context scaling" 的 40% handoff cap 封顶,应改述为"相对静态前缀(~5-10K tok)的倍率,随 active-window×迭代数 scale",对大窗口长 loop 巨大、对短 turn 缩向静态前缀;且朴素断点会因每轮 tool-result append + 每次 40% handoff **self-invalidate**,断点必须钉在"上一轮持久化 tape 前缀的最后一条消息"上。tool-schema 70% 削减数字**未按真实 ~40 工具重测前不承诺**。

**两处口径 nit:** (a) design §0.3「软拒绝、不杀进程」**过度更正**了 —— 端到端 async cap 实为 **kill-and-refuse**(tools.rs:2642-2643:spawn shell → register 返回 false → `terminate` 杀 shell → 返回 Err → model 重试);`register()` 自身确实只返 false,但调用方杀了进程。(b)「eli is NOT multi-agent」头条加限定:**durable/async 形态 = job-queue;synchronous 形态 = bounded orchestrator-worker fan-out;两者都不是 peer-to-peer**。

**修正后的动手顺序:**
1. 两个 token P0(history cache 断点 + 每迭代 O(n) 扫描提升)—— 无依赖、可并行、可逆,先做 de-risk;断点改动与 decision-injection 移出缓存区合并,并同时上 cache hit/miss 计数器。
2. `set_inbound_injector` warn-and-overwrite 守卫(3 行,独立)。
3. 回流接线**只接到已有 subagent 路径** + task_id/intent additive 字段(修 telephone-game,这才是 design Phase-1 真正赢点)。
4. `worker + subscriber + cap→enqueue` → 打包为一个 evidence-gated Phase-2 决策,触发条件 = 观测到 session 积累待 drain 的 todo;届时 worker 先升 subagent 标准,subscriber 自带 reaper/Lagged-reconcile/dedup。

---

## 1. 架构能否支撑后续迭代

### 总体评级: 🟡 黄(偏绿) — grow-from-hooks 基本成立

心智模型成立:**multi-agent 可以 layer 在 hooks 之上而不重构核心**。三份架构审计在这点上**一致**:Envelope 是裸 `serde_json::Value`(types.rs:11),12 个 hook 点覆盖了 roadmap 需要的每一处拦截(hooks.rs:259-361),`register_plugin` 走 `ArcSwap.rcu`(framework.rs:80)支持运行时加 plugin,last-registered-wins 让 multi-agent plugin 干净覆盖 builtin。result-回流 的形状已被 async subagent 工具**实证跑通**(tools.rs:2745-2759:捕获 `inbound_injector()` → spawn → 注入合成 envelope)。

但**"能 layer" ≠ "已就绪"**:durable-job-queue 的下半身(消费侧)整套建好却**完全没接线**。这是黄而非绿的原因。

### 强项(现有 primitive 给的 headroom)

| Primitive | 给了什么 | Evidence |
|---|---|---|
| Untyped Value envelope | `task_id`/`intent` 纯加性、**零 migration、零序列化版本号**;缺失即 `None`/default,走的是已被测过的路径 | types.rs:11; builtin/mod.rs:376; envelope.rs:469 (`test_outbound_message_defaults_on_missing_fields`) |
| 12 hook 点 + last-wins | load_state/build_user_prompt 读 task_id/intent;run_model 可按 task-kind 路由子 loop;wrap_tool 门控 A2A 工具面 | hooks.rs:259-361, 418-421 (reversed) |
| ArcSwap.rcu plugin 注册 | 运行时加 plugin 不阻塞在途 turn | framework.rs:46,80 (`hook_runtime: ArcSwap<HookRuntime>` + `.rcu`) |
| Taskboard schema | `kind` 故意是裸 String(mod.rs:46 注释"避免 calcification");`Status::Blocked{waiting_on}`、`Claimed`、富 `Failed{retries,tool_trace}` 全在;status_json 列已能装下依赖边 | taskboard/mod.rs:42-91 |
| 原子 claim_next | 单行 CAS(`UPDATE...WHERE state='todo'`,store.rs:622),double-claim 已被 `claim_atomicity` 测证伪;优先级排序(`ORDER BY priority DESC, created_at ASC`,store.rs:598)、active_count、per-session rate-limit 都有 | store.rs:587-639, 739-761, 345-355 |
| Sidecar versioned contract | `eli.sidecar.v1` 版本常量 + `#[serde(flatten)] extra` 前向兼容 + golden fixture + CI drift 守卫(`test_schema_bundle_matches_committed_schema`),10 测全绿 | sidecar_contract.rs:7,44-52,157-160,291-314 |

Tape replay 安全性是隐藏强项:路由 Envelope **不落盘**,落盘的是 `{role,content}` 消息形(entries.rs `TapeEntry.payload: Value`),loader `entry_from_payload` 宽容(store.rs:457-477),所以加 task_id/intent **不可能破坏老 tape replay** —— infinite-context invariant 守住。(本次复核确认:路由 envelope 不触盘,加性字段对老 tape replay 是 sound 的。)

### 承重墙 / iteration blockers(按 phase 标注)

**🔴 [Phase 1, 真阻碍] Taskboard 事件总线零生产订阅者 —— `TaskEvent::Completed/Failed` fire-into-void。** `store.subscribe()`(store.rs:102)是其唯一定义处;本次 grep 复核确认 `crates/eli/src` 内 `.subscribe()` **仅此一处调用**,生产代码无人订阅。store 每次 mutation 都 broadcast,但没人听 → `task.update(result=...)` 完成一个任务**不产生任何回流**,提交 session 永不被通知。**最小改法**:gateway/host 启动处起一个长驻订阅任务,`subscribe()` → 桥接 `Completed/Failed → inject_inbound`,复用 subagent monitor 的 envelope 形状。纯加性,framework.rs/hooks.rs 零改。**注意** broadcast 容量 128(store.rs:61),慢/缺订阅者会 Lagged 丢事件 —— 今天无害(无订阅者),回流依赖它后是正确性风险。

**🔴 [Phase 1, 真阻碍] `TaskWorker::run` 生产环境从不 spawn。** 本次复核:`TaskWorker` 仅出现在 worker.rs(定义)与 `tests/taskboard_integration.rs`。无任何 builtin/channel/gateway/chat 启动路径构造它。后果:`task.create` 写一行 Todo 但**没人 claim/执行**,claim_next 只被测试触发。taskboard 今天只是被动 recitation 表(agent_request.rs:276-291 只读)+ 手动 `task.update` API。**最小改法**:从已持有 `CancellationToken` 的 gateway/chat 启动 glue 里 spawn `TaskWorker::run(cancel)`,env flag 灰度。worker 已完整且有测试,纯加性。

**🟡 [Phase 3, 真阻碍 / 今日已是 latent foot-gun] 单槽全局 `INBOUND_INJECTOR`。** 本次复核确认:`Mutex<Option<InjectInboundFn>>`(control_plane.rs:187),`set_inbound_injector` **无条件覆盖、无 warn/refuse**(control_plane.rs:192),chat 与 gateway **各自注册不同策略**(chat.rs:14-17 直调 `process_inbound`,gateway.rs:450 走 ingress mpsc)。今天安全只因 chat/gateway 是互斥子命令(一进程一角色)——**但这是 latent foot-gun,不是纯未来风险**:一旦同进程出现任何第二个注册方(双路径并存,或 Phase 3 任意新入站源),最后一次 `set_inbound_injector` **静默胜出、无任何告警**,后台 job 结果会经**错误传输**投递。这是结构天花板:**它不限吞吐(每次 spawn clone 同一 Arc),但把投递路由的 distinctness 限死为 1**。**最小改法**:升级为 source-keyed registry/router;或至少**立刻**先让 `set_inbound_injector` 在覆盖时 warn/refuse(这是当下就该加的最小护栏,不等 Phase 3)。完整升级 touches 所有 call site 但 reversible —— **Phase 3 前必须 sign-off**。

**🟡 [Phase 3, 真阻碍] 全局 `OnceLock TASK_STORE` —— 一进程一块板。** 所有 `task.*` 工具走 `require_task_store()`/`task_store()`(定义 taskboard/mod.rs:17)。Phase 1/2 串行单板正确且方便,但 multi-tenant/multi-workspace 拿不到隔离板,得改成 keyed registry 并穿线 store handle,touches 每个 task.* 工具。Reversible,但**在 surface 固化前决定**。

**🟡 [横切] Backpressure 没有 hook seam,是 kill-and-refuse 不是 enqueue。** `process_inbound` 被每个入口直调,gateway 每条 inbound `workers.spawn` 进 JoinSet **无全局并发上限/semaphore**(gateway.rs:499);per-session Agent Mutex 只串行化同 session(builtin/mod.rs:113-131)。并发 cap 是 kill-and-refuse(tracker 满 → 杀**刚 spawn 的** shell + 报错让模型重试,tools.rs:2642-2651)**而非排队**。**最小改法**:cap 满时改 `task.create` 一个 Todo,bounded worker pool(size=max_concurrent)`claim_next`。关键认知:**backpressure 是入口/worker glue 的事,不是 hook 点** —— 必须写进设计 note,别误当缺失的 hook。

**🟡 [Phase 2, 待补] 依赖边是 schema-only。** `Status::Blocked{waiting_on}` 本次复核确认**只在 mod.rs:88-90 类型定义 + mod.rs:106 match-arm 字符串出现,从不被构造**(command_semantics.rs 里的 "Blocked:" 是无关字符串字面量)。claim_next **完全无视 waiting_on**(store.rs:598-600 WHERE 子句无依赖谓词)→ 会 dispatch 未满足前置的任务。**最小改法**(无需 migration,enum/列/parent 索引都已在):(a) 加工具设 `Blocked{waiting_on}`;(b) claim_next WHERE 子句排除 waiting_on 指向非终态的 todo;(c) Completed 事件触发 unblock。

**🟢 [低危,Phase 3 前留意]** Gateway injector 路径有损:`channel_message_from_envelope` 硬编码 `is_active:false`、`kind:Normal`,**丢 media_parts**(`media:Vec::new()`,gateway.rs:248-291);chat 路径无损(裸 Value 透传)。**但 context 字段被保留**(gateway.rs:274-278),所以下面"用 context 字段标准化"的建议是 sound 的。**结论性建议**(两份审计一致):inter-agent envelope **标准化用 context 字段**(双路径无损 round-trip 并被存进 `_inbound_context`),不要用 top-level kind/media。

> **审计分歧点(已核实并裁决)**:`hooks_extensibility` 把回流写成"wire `TaskEvent::Completed → inject_inbound`";`coordination_substrate` 更准确地指出**回流已经 work,但走 per-spawn monitor、不碰 taskboard 事件流**(tools.rs:2654 在 launch 时捕获 inject_fn;tools.rs:2683 spawn monitor;2745-2759 注入)。裁决:回流机制存在且已生产跑通;让 **taskboard 起源的** job 走同样回流,需要新加订阅者 —— 形状相同、可行。两者不矛盾,后者更精确。
>
> **审计一处措辞需修正**:`hooks_extensibility` 称 `builtin/mod.rs:5` 引用了不存在的 `taskboard_plugin.rs`。本次复核:mod.rs:5 实为 `pub(crate) mod coding_plan`(真实模块,coding_plan.rs 存在);`taskboard_plugin.rs` 确实不存在,但**并未在 mod.rs:5 被引用**。Phase 1 plugin 仍需新建该文件,结论不变,但别照搬"已被引用"的说法。

### 逐 Phase 充分性表

| Phase | 评级 | 关键阻碍(最小改法) |
|---|---|---|
| **Phase 1**(result回流 + task_id/intent + 串行 worker) | 🟢绿(机制就绪,差接线) | 仅缺两处加性接线:① spawn 长驻订阅者桥接 `Completed→inject_inbound`;② startup spawn `TaskWorker::run`。task_id/intent 加为 **context 字段**(双路径无损)。framework.rs/hooks.rs 零改。 |
| **Phase 2**(schema validation + guardrails) | 🟢绿(有现成模板) | 扩 `sidecar_contract.rs` 加 A2A/task 消息结构(JsonSchema-derived、版本门控),在 classify_inbound/load_state hook 里校验返回 `HookError`;guardrails 进 `wrap_tool`(已由 MiddlewareChain 用,builtin/mod.rs:736)。依赖边需补 Blocked/waiting_on 写入路径。 |
| **Phase 3**(A2A surface) | 🟡黄(两根承重墙需先拆) | 单槽 `INBOUND_INJECTOR`(control_plane.rs:187)+ `OnceLock TASK_STORE`(taskboard/mod.rs:17)必须升级为 source-keyed registry;并发账目改为 session-aware(taskboard 已带 `session_origin`,tracker 不行,会跨 session 饿死)。**两者 reversible 但 touch 全 call site → 需 sign-off**。 |

---

## 2. token 效率

### 总体评级: 🟡 黄 — 地基对,两个大漏点

**最大杠杆 / #1 确认项: prompt caching 是真 SENT,不是只被 measured。** 这是最该确认的,本次已亲自复核:anthropic 适配器的 `mark_cached()`(anthropic.rs:91-98)写 `cache_control {"type":"ephemeral"}`,被**恰好调用两次** —— 最后一个 tool(anthropic.rs:64)+ 最后一个 system block(anthropic.rs:126);`build_system_value` 仅在 `cache=true` 时把最后一个 block 标记。由 `request.prompt_cache` 门控,生产路径 `prompt_cache: true`(execution.rs:427,对照测试里 false)。这是"measured but never sent"失败模式的**反面**。OpenAI 服务端自动缓存,无需动作。

前缀也工程化做了 cache-stable:Runtime 段用**日期精度**(非秒,prompt_builder.rs:327,代码注释记录了 M_e.10 秒精度回归把前缀缓存打爆),且 Runtime 放 system 最后,日切只失效尾部。tool 快照按 registry fingerprint memoize(`model_tools_cached`,**crates/eli/src/tools.rs:12-59**,byte-stable)。观测也全打通:`cache_hit_ratio` 写进 tape + tracing(agent/agent_run.rs:694-732)。

### Hotspots 表(按 token 影响排序)

| # | Hotspot | 量级 | Evidence | 修法 |
|---|---|---|---|---|
| 1 | **对话历史从不打 cache 断点** —— Anthropic 允许 4 个断点,只用了 2(system+tools)。tool-loop 每轮从 tape 重建且单调增长,**前缀一破整个(最大的)消息块全价重计**。(注:tool_loop 的 `tail_reminder` "ephemeral" 是非持久上下文语义,**不是** Anthropic cache_control 标记) | 长 agent loop 中历史常**远超** ~4-5K 静态前缀,潜在节省比已捕获的 system+tools 缓存**更大** | anthropic.rs:64,126(全 nexil 仅这 2 处 cache_control);tool_loop.rs:289-301 | 在**稳定历史尾部**(本轮新 tool round 之前、ephemeral tail_reminder 之前)加一个 cache_control 断点 → 历史从全价降到 ~0.1x。**🔴 最高杠杆** |
| 2 | **全量 tool schema ~40 工具每轮全发** —— 描述字面量实测 **~8,264 字符(~2,066 tokens)**,含 param schema 估 **~3.8-5K tokens/轮**(注:该上限是估算,非逐字节测量;仅描述部分被实测)。最大杠杆 P2 "Lazy Tool Groups"(目标 ~1,200 vs 全量 ~5,000,~70% 削)**显式 deferred** | cache 命中软化(~0.1x),但**每个 session 首轮 + 5min 闲置后 eviction + 任何 tool-set 变更**付全价 | tools.rs:286-339;docs/plans/builtin-tools-perf-context.md:63-87 | 恢复 P2;**最低风险切片**:**10** 个 `evolution.*` 治理工具(实测 ~2,147 字符 ≈ **~536 描述 tokens,占描述总重 ~26%**)移出常驻集,曝露为 skill(sidecar 工具已是此模式)。**🟡** |
| 3 | **decision 注入每轮改 system 前缀** —— `inject_decisions_into_system_prompt` 把 "Active decisions" 块 append 到最后一个 system message;system block 正是 cache 断点 | decision add/revoke 那轮触发 cache_creation(1.25x)写 | tool_loop.rs:425-426;decisions.rs:37-59;anthropic.rs:126 | 把 decision 注入移到 cache 断点**之后**(独立 trailing message),或仅在 decision 集真变时重注 |
| 4 | **plan/read-only 模式 + tool filter 静默打破 tools 缓存前缀** —— 走 `model_tools(&tools)`(过滤、未缓存)而非 memoized snapshot | 中途切 plan mode 强制 tools-miss,重付 ~4-5K tool 块 | agent_request.rs:354-359 | 行为正确(模式真改了可用工具),量 hit ratio 时知情即可;**非 bug** |
| 5 | **TRIM_NOTICE 注入移位前缀** —— `aggressive_trim`/`inject_trim_notice` 触发时把 notice prepend 到首个存活 message(或 push 新 assistant 消息),shift 所有下游字节偏移,触发瞬间作废任何前缀缓存 | 仅 trim 触发那轮 | context.rs:216,232-242 | summarize-and-anchor 而非改首个存活 message,保前缀 byte-stable |
| 6 | **2000-byte subagent tail** —— 仅留最后 2000 **字节**(~500 tokens),tail-only。**已用 `ceil_char_boundary` 切,UTF-8 安全、不会 panic** | 强 token bound;问题是**CJK 下 2000 字节仅 ~666 字真内容**(under-inclusion,非正确性 bug) | tools.rs:47,251-255 | head+tail 选项(保住开头 summary);若想多纳 CJK 内容,bound 改 char-based。**注:现有代码已 boundary-safe,这是质量改进而非修 panic。🟢 低危** |
| 7 | **回流 payload 降级为 prose content 字符串** —— `build_completion_message` 格式化人读字符串当 content,丢弃结构化 `result_json` | 高 fanout fan-in 时是主导上下文成本(N 个完成的 subagent 各注入完整格式消息 + 截断尾) | tools.rs:2745-2759 | 回流保留结构化 result_json,fan-in turn 消费 typed 结果免重 parse |

### Context scaling: bounded,不是 linear

**关键正面结论(两份 token 审计一致)**:每轮 token 用量是 **BOUNDED,不随全历史线性增长**。`build_tape_messages` 读全 tape 后 `slice_entries_by_anchor` 只留**最后 anchor 之后**的条目(LastAnchor 默认,tool_loop.rs:401-429;helpers.rs:70-103)→ prompt 规模随 work-since-handoff 缩放,非总对话长度。`apply_context_budget` 是第二道硬 cap(超 ~400K 字符 / context_window*4 时只留最后 2 轮)。40% auto-handoff(`ELI_HANDOFF_THRESHOLD_PCT` 默认 40,从 70 降下来,**agent/agent_run.rs:399**)把活跃窗结构性压在模型上限远下方。这是 unbounded-tape 系统的**正确设计**。

**但有一个非 token 的 CPU/latency 尾巴**:窗口化历史在**每个 tool-loop 迭代**从全 tape 扫描重物化 —— `_prepare_messages`(tool_loop.rs:289)在 loop 内被调 → `build_tape_messages` → `fetch_entries` 扫全 tape(无 anchor pushdown,tool_loop.rs:407)+ `collect_active_decisions` 是**第二次** O(n) 全扫(tool_loop.rs:425;decisions.rs docstring 直言"Scans ALL entries regardless of anchor slicing")。250 迭代的大 tape 上是 O(iters × total_entries)。不是额外 LLM token,但是热路径浪费,随 tape 增长。**修法**:loop 前一次性 fetch + slice + collect_decisions,把结果传进每轮(单轮内 pre-anchor 历史与 decision 集不变)。

> **项目 plan 文档有一处过期数字(本次复核新增)**:`docs/plans/builtin-tools-perf-context.md:63-87` 把工具宇宙描述为"all 21 schemas / 21 tools",但实际注册量是 **~40**(38 builtin + sidecar)。该文档的 "~1,200 vs ~5,000" 估算是按已过期的 21 工具基线 sized 的,**可能低估今日全量 schema 成本** —— 上面 hotspot #2 的 ~3.8-5K 估算同样继承了这一不确定性,落 P2 时应按真实 ~40 工具重新 byte-measure。

---

## 3. 综合建议: 排序后的 top moves

### 为了能迭代(架构)

| 优先 | Move | Phase | Reversible? | Sign-off? |
|---|---|---|---|---|
| **P0** | 起长驻 `TaskEvent` 订阅者(gateway/host startup):`subscribe()` → 桥接 `Completed/Failed → inject_inbound`,carry `session_origin` 作 session_id,专用任务防 Lagged。**最高杠杆单线** —— 把 taskboard 从被动 tracker 变真 durable job queue | 1 | ✅ 纯加性 | 否 |
| **P0** | startup spawn `TaskWorker::run(cancel)`(复用现有 CancellationToken),env flag 灰度 —— worker 已完整有测试 | 1 | ✅ 纯加性 | 否 |
| **P0(立刻)** | `set_inbound_injector` 覆盖时 warn/refuse(control_plane.rs:192)—— **当下就该加的最小护栏**,堵住单槽 injector 的 silent-clobber latent foot-gun,不等 Phase 3 | 1 | ✅ 纯加性 | 否 |
| **P1** | task_id/intent 加为 envelope **context 字段**(非 top-level),双路径无损 round-trip;读经现有 ValueExt default。补一个镜像 `test_outbound_message_defaults_on_missing_fields` 的单测 | 1 | ✅ 零 migration | 否 |
| **P1** | 并发 cap 从 kill-and-refuse 改 enqueue:满时 `task.create` Todo + bounded worker pool `claim_next`。backpressure 写进 worker/injector glue **不是 hook**,文档化 | 1-2 | ✅ 加性 | 否 |
| **P2** | Phase 2 校验:扩 `sidecar_contract.rs` 加 A2A/task 结构(版本门控 + golden fixture),hook 里 `HookError`;guardrails 进 wrap_tool。依赖边补 `Blocked/waiting_on` 写路径 + claim_next 谓词 + unblock 触发 | 2 | ✅ 加性 | 否 |
| **P3 决策点** | 单槽 `INBOUND_INJECTOR`(control_plane.rs:187)+ `OnceLock TASK_STORE`(taskboard/mod.rs:17)→ source-keyed registry(完整升级);并发账目 session-aware(迁到带 `session_origin` 的 taskboard) | 3 | ✅ 但 touch 全 call site | **⚠️ 需 sign-off**(Phase 3 前,surface 固化前) |

### 为了 token 效率

| 优先 | Move | Reversible? | Sign-off? |
|---|---|---|---|
| **P0** | 在**稳定历史尾部**加 cache_control 断点(anthropic.rs,4 个只用 2 个)→ 历史从全价降 ~0.1x。长 loop 最大剩余赢点 | ✅ | 否 |
| **P0** | loop 前一次性 fetch tape + slice + `collect_active_decisions`,消除每迭代两次 O(n) 全扫(tool_loop.rs:289,407,425) | ✅ | 否 |
| **P1** | 恢复 P2 Lazy Tool Groups;**先切** **10** 个 `evolution.*` 移出常驻集(曝为 skill)→ 每个 miss 轮省 ~536 描述 tokens + schema。落地前按真实 ~40 工具重测 baseline(plan 文档 21 数字已过期)。**注**:需新建承载文件(如 `builtin/taskboard_plugin.rs`,当前不存在) | ✅ | 否 |
| **P1** | decision 注入移出 cached system block(断点之后 / 仅变更时重注),止血每轮前缀失效 | ✅ | 否 |
| **P2** | surface cache 统计到 REPL:`TurnUsageInfo` 加 cache_read/write/hit_ratio(types.rs:31),framework.rs:245 透传,cli/mod.rs print_usage 打印 —— 让 cache-bust 回归在 REPL 可见(守 date-precision 类修复) | ✅ | 否 |
| **P3** | subagent tail 改 char-based + head+tail(现有 `ceil_char_boundary` 已 UTF-8 安全,这是质量改进非修 panic);回流保结构化 result_json | ✅ | 否 |

**底线**:架构地基对 —— Phase 1/2 是"接线 + 加 plugin",**不是重构**;Phase 3 只有两根承重墙(两个全局单例)真逼改核心,且都 reversible、可提前 flag。token 侧 caching **真发了**(已复核确认 cache_control ephemeral 在 anthropic.rs:91-98 经 mark_cached 发出、生产 `prompt_cache: true`),但**历史无断点 + tool schema 膨胀**两个大漏点未补,且都有明确最小改法。两份审计的唯一实质张力(回流是否需要 wire Completed)已裁决:回流已跑通,taskboard 起源 job 走同样回流是加性订阅者工作。

相关文件:`crates/eli/src/control_plane.rs:187,192`、`crates/eli/src/taskboard/{mod.rs:17,88-90,store.rs:61,102,598,622,worker.rs}`、`crates/eli/src/builtin/cli/{chat.rs:14-17,gateway.rs:248-291,450,499}`、`crates/eli/src/builtin/tools.rs:{47,251-255,286-339,2642-2651,2654,2683,2745-2759}`、`crates/eli/src/tools.rs:12-59`、`crates/eli/src/builtin/agent/agent_run.rs:{399,694-732}`、`crates/eli/src/builtin/agent/agent_request.rs:354-359`、`crates/nexil/src/providers/anthropic.rs:{64,91-98,126}`、`crates/nexil/src/core/execution.rs:427`、`crates/nexil/src/llm/{tool_loop.rs:289-429,decisions.rs}`、`crates/eli/src/context.rs:216,232-242`、`crates/eli/src/framework.rs:46,80`、`crates/eli/src/prompt_builder.rs:327`、`crates/eli/src/sidecar_contract.rs:7,44-52,157-160`、`docs/plans/builtin-tools-perf-context.md:63-87`。

---

## 排序后的行动清单(tight)

1. **[P0·架构]** Spawn 长驻 `TaskEvent` 订阅者,桥接 `Completed/Failed → inject_inbound`(carry `session_origin`,防 Lagged)。把 taskboard 变真 durable job queue。纯加性。
2. **[P0·架构]** Startup spawn `TaskWorker::run(cancel)`,env flag 灰度。worker 已就绪有测试。纯加性。
3. **[P0·架构·立刻]** `set_inbound_injector` 覆盖时 warn/refuse —— 当下就堵 silent-clobber latent foot-gun,不等 Phase 3。
4. **[P0·token]** 在稳定历史尾部加第 3 个 cache_control 断点(4 个只用 2 个)→ 历史全价降 ~0.1x。长 loop 最大剩余赢点。
5. **[P0·token]** Loop 前一次性 fetch+slice+collect_decisions,消除每迭代两次 O(n) 全扫(tool_loop.rs:289/407/425)。
6. **[P1·架构]** task_id/intent 加为 envelope context 字段(双路径无损),补默认值单测。
7. **[P1·token]** 切 10 个 `evolution.*` 出常驻集、曝为 skill(每 miss 轮省 ~536 描述 tokens);落地前按真实 ~40 工具重测 baseline。
8. **[P1·token]** decision 注入移出 cached system block(断点之后或仅变更时重注)。
9. **[P1·架构]** 并发 cap 改 enqueue(`task.create` Todo + bounded worker pool claim_next),文档化 backpressure 属 glue 非 hook。
10. **[P2]** Phase 2 校验扩 `sidecar_contract.rs` + 依赖边写路径;REPL surface cache 统计。
11. **[P3·决策点·需 sign-off]** 两个全局单例(`INBOUND_INJECTOR` + `OnceLock TASK_STORE`)→ source-keyed registry;并发账目 session-aware。Phase 3 前 sign-off。
12. **[P3·token]** subagent tail 改 char-based + head+tail(现已 UTF-8 安全,纯质量改进);回流保结构化 result_json。