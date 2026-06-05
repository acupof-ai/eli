<!-- Produced 2026-06-05 via a research workflow (10 agents): landscape survey + eli code/plan grounding + adversarial critique + revision. All eli code claims ground-truthed against the tree. -->

# 多 Agent 系统中 Agent 间信息交互的产品设计

> ⚠️ **Review 修正(2026-06-05 四透镜复核)**:本稿的 Phase-1 "把 `TaskEvent::Completed → inject_inbound` 接通 + 激活串行 worker" 经复核**降级为 evidence-gated Phase 2** —— 回流应复用已工作的 async-subagent monitor(`tools.rs:2745`),不点亮更原始的 `TaskWorker`(无 worktree、2000-byte 截断,反而重引入 telephone-game)。另两处 nit:(1) §0.3「软拒绝、不杀进程」**过度更正**,端到端 cap 实为 kill-and-refuse(tools.rs:2642-2643);(2) "durable job-queue, NOT multi-agent" 头条应加限定 —— synchronous 形态仍是 bounded orchestrator-worker fan-out。完整修正见 [`2026-06-05-arch-sufficiency-token-audit.md`](2026-06-05-arch-sufficiency-token-audit.md) 的「Review 修正」段。

> 范围：以 eli 现有架构为地基，给出"agent 之间该如何交互信息"的产品-系统设计草案。本文是**已经过一轮 ground-truth 校正的草案**，立场鲜明、决策导向（architect posture：输出决策，附驱动该决策的 tradeoff），但已剔除"在现有 primitive 上不可建"的核心机制和一批超前工程。
>
> **一句话结论（修订后）：eli 不需要新建一套 agent-to-agent 消息总线，也不需要把 tape 改造成跨 agent 的共享 blackboard。eli 跨进程子 agent 的真相源**就在它已经有的两个东西里**：(1) git worktree + diff/commit 产物串（`collect_artifacts`，唯一能穿越子进程边界的"共享黑板"），(2) taskboard 这个**持久任务队列 / job-board**（claim + serial worker + `waiting_on`）。正确的多 agent 信息交互模型是一个 durable job queue + 串行 worker，而不是"多 agent 并发共享上下文"。最高杠杆、最低风险、最该先做的是：把 `TaskEvent::Completed → inject_inbound` 接通让结果回流、加 additive 的 `task_id` + `intent` 两个便宜字段、把 2000-char tail 降级为 preview 并把 git-artifacts 串提升为主产物指针。A2A 全套信封对象模型、typed parts[]、provenance/merge 仲裁层、依赖 DAG executor、完整编排 UX、sidecar v2/AgentCard——全部 evidence-gated 推迟，因为它们是为一个 eli 目前**结构上不存在**的"并发共享上下文多 agent 群体"准备的机器。**

> 修订说明（与上一稿的关键差异）：
> - **撤回原"核心洞察"**：原稿主张"subagent 写 forked tape、回传 `tape_ref` pointer、parent merge 选中条目"。经核对代码，**这条主线在现有 primitive 上不可建**（见 §0 事实更正）。`fork_tape` 是同一 task、`task_local`、闭包作用域、退出即丢弃的**上下文隔离 scope**，不产生任何可寻址、可持久的 side-tape，更无法穿越 OS 进程边界——而 eli 的**主**子 agent 路径正是跨进程 CLI 子进程。本稿改以 **git worktree + diff 为跨进程产物通道**、**taskboard 为持久协调/durability 层**。
> - **新增 §0**：集中列出所有 ground-truth 更正，避免后续章节继续引用错误前提。
> - **拓扑补完**：新增"durable job-queue / serial-worker（Sidekiq/Celery/Temporal-lite）"——这才是 eli 实际落地形态的最贴切先验。
> - **trim**：A2A 对象模型、provenance/仲裁层、依赖 DAG executor、§5 全套编排 UX、sidecar v2 全部从"建议"降级为"远期、evidence-gated"。

---

## 0. 事实更正（Ground-Truth Corrections）——先把地基钉死

> 以下每一条都对照 eli 真实代码核对过。后续章节在这些事实上构建。凡上一稿与代码冲突处，以本节为准。

1. **`fork_tape` 不是"持久 side-tape"，是"同 task 的上下文隔离 scope"。** `ForkTapeStore::fork`（`builtin/store.rs:119`）新建一个 `InMemoryTapeStore`，用 `tokio::task_local!`（`CURRENT_STORE`/`CURRENT_FORK_TAPE`，`store.rs:18`）把它 scope 进一个闭包；闭包退出后，**除非 `merge_back=true` 否则条目直接丢弃**（`store.rs:137`）。它不返回任何 `tape_ref`，没有可寻址的副本留存。`task_local` 是 tokio 任务局部变量，**不跨 OS 进程**。所以：
   - "subagent 写 forked tape、回传 pointer、parent 按需 `tape.search` 拉"——**没有可指向的对象，pointer 必然悬空**。
   - 跨进程 CLI 子 agent（`shell_manager` 起的 `claude`/`codex`/`kimi` 子进程，主路径）**物理上无法写进 parent 的 fork store**。
   - 因此原稿"tape 作为 inter-agent blackboard"在 parent↔child 之间**两种模式都没有可工作的代码路径**。

2. **in-process fallback 也不共享 parent tape。** `fallback.rs:31-37`（`run_in_process`）为子 agent 新建一个全新 `InMemoryTapeStore` + 自己的 `ForkTapeStore` + 独立的临时 `tapes_dir`。它和 parent 的 tape store **零共享**。所以 in-process 模式同样不把 child 写入接回 parent tape。

3. **并发上限是"软拒绝 spawn"，不是"杀进程"。** `AgentTracker::register`（`tracker.rs:84`）在 `can_spawn()` 为假（`running_count >= max_concurrent`，默认 5）时**返回 `false`**——拒绝再 spawn，不 panic、不杀进程。原稿"直接 error 杀进程"是错的，这削弱了"必须上队列"的紧迫性论证：真实失败模式是"spawn 被婉拒"。

4. **`reply_to_id` / `kind` / `is_active` 不是 in-process envelope 的字段。** in-process 通用信封只有 `ValueExt`（`envelope.rs:9`）的 `field`/`field_str`/`content_text`/`normalize_envelope`/`unpack_batch`——**没有 `reply_to_id`**。`reply_to_id`（`sidecar_contract.rs:39`）、`kind`/`is_active`（`SidecarChannelMessage`，`sidecar_contract.rs:63-65`）都是**类型化 sidecar 契约**的字段，外加 webhook.rs 里一个松散的 `context.reply_to_id` 约定。**不要把类型化 sidecar 契约和无类型 in-process envelope 混为一谈**——这是上一稿反复犯的错，它系统性高估了 envelope"已经携带"了什么。

5. **跨进程真正已工作的产物通道是 git，不是 tape。** `AgentResult` 有一个 `artifacts: String` 字段（`tracker.rs:34`），由 `collect_artifacts`（`tools.rs:163`）填充——它跑 `git log pre..cur` + diff stat，把子 agent 在 worktree（`subagent/worktree.rs`）里的提交/改动序列化成字符串回传。**对 CLI 子 agent（主路径）而言，文件系统 / git 就是那块唯一能穿越子进程边界存活的共享黑板。** 这是 eli 真正已经做到"原稿想要的事"的机制，上一稿严重低估了它。

6. **A2A / FIPA / Anthropic / Cognition / MAST 的数字是带条件的外部证据，不是 eli 的常量。**
   - FIPA-ACL 唯一严格必填的是 performative（intent）——这点对。但说"A2A 只是 FIPA 改名、形状不变"是误导：A2A 是 task-centric（Task/Artifact/Message + TaskState 生命周期 + AgentCard 发现），**FIPA 没有 artifact/task 对象模型**，这是 A2A 的新增物，不是改名。
   - A2A AgentCard 的规范发现路径是 **`/.well-known/agent.json`**，不是上一稿写的 `agent-card.json`。
   - Anthropic 的"15x token""token 用量解释 80% 性能方差"来自其 research-system 博客，**特指 breadth-first 研究型负载**，不是通用多 agent 常量。Flappy-bird/Mario 是 Cognition 的**示意性轶事**，不是测得的失败率。
   - MAST 的 `+15.6%`、Kappa 0.88、1600+ traces、FM-x.x 百分比是跨**异构开源框架**的聚合发现，干预增益绑定具体系统，**不保证迁移到 eli**。本稿引用它们时一律标注"方向性证据，非保证收益"。

---

## 1. 问题框架（Problem Framing）

"agent 间信息交互"不是一个问题，是**五个正交问题**叠在一起。把它们拆开是本文的脊柱——每一根脊柱可以独立做技术选型，组合起来才是一个完整系统。

```
┌─────────────────────────────────────────────────────────────────┐
│  agent 间信息交互  =  5 个正交轴                                    │
├─────────────────────────────────────────────────────────────────┤
│ (a) Transport / Topology   谁能跟谁说话？(wiring: 链/星/树/板/总线/队列)│
│ (b) Message Envelope        消息长什么样？(intent/ID/typed content) │
│ (c) Context & Memory        共享多少上下文？(隔离 vs 共享/摘要 vs 全文)│
│ (d) Coordination / Control  谁决定下一步？(handoff/supervisor/queue) │
│ (e) Human-facing Surface     人怎么看见这一切？(plan/trace/interrupt)│
└─────────────────────────────────────────────────────────────────┘
```

为什么必须拆开：研究里所有混乱都来自把这些轴混为一谈。例如 eli 代码里**两个完全不同的东西都叫 "handoff"**——`tape.handoff`（轴 c：单 agent 上下文溢出时的 intra-session 压缩）和 async subagent 结果回注（轴 a+d：inter-agent 结果投递）。这是典型的"轴没拆开"导致的词汇污染。

每个轴的取值范围（后面逐一展开）：

| 轴 | 取值谱系（从轻到重） |
|---|---|
| (a) Topology | sequential chain → orchestrator-worker (star) → hierarchical tree → **durable job-queue + serial worker** → blackboard → pub-sub bus → P2P mesh |
| (b) Envelope | free-text string → flat JSON → typed envelope (intent + IDs + typed parts) → 签名/带 provenance |
| (c) Context | 完全隔离 (fresh window) → 摘要回传 → **artifact/pointer 回传（git diff 串）** → 全共享 tape（仅单进程内） |
| (d) Coordination | 命令式 delegation → handoff-as-tool → supervisor 路由 → **taskboard claim + queue** → contract-net 竞标 |
| (e) Surface | 隐藏一切（黑盒）→ 折叠默认+按需展开 → live progress tree → plan-gate + interrupt + replay |

---

## 2. 设计空间与决策轴（Design Space & Decision Tables）

### 2(a) 拓扑（Topology）

> 治理性 tradeoff（一句话预测所有 con）：*"去中心化提升韧性但放大协调成本；中心化提升效率但制造单点故障。"* 连通度既加速信息传播也加速错误/幻觉传播——加 agent/加边**不是免费的好事**。

| 拓扑 | 何时用 | 代价 |
|---|---|---|
| **Sequential chain** | 固定已知阶段（outline→draft→review） | 早期错误污染全链；无并行；脆弱 |
| **Orchestrator-worker (star)** | runtime 决定子任务、单协调者可接受、read-heavy 收集 | hub 是瓶颈+单点故障；context 聚合压力集中 |
| **Hierarchical tree** | 多专业、需 bounded context、可解释路由、规模大 | 顶层过载成瓶颈；深树拉长 trace |
| **Durable job-queue + serial worker**（Sidekiq/Celery/Temporal-lite） | **持久任务、需可恢复/可观测/backpressure、结果异步回流、单 worker 串行消费就够** | 队列/worker 需运维；串行 = 无并行吞吐；需 claim 原子性 + stale 检测 |
| **Blackboard** | 协调者无法预知谁相关、能力异构、要单一可审计真相源 | 板是争用点+瓶颈；写冲突需 reconcile；**跨进程时"板"只能是文件系统/git** |
| **Pub-sub bus** | 高吞吐、事件驱动、多 agent 按 role 订阅、需独立扩缩 | 跨 topic 因果难追；要运维 broker |
| **P2P mesh** | 不接受中心权威、要最大韧性、小 n | O(n²) 协调开销；最弱可观测性；错误对等传播 |
| **Debate/voting** | 难推理+清晰评判标准、冗余值这个钱 | agents×rounds 成本；confident-wrong 羊群效应 |
| **Contract-net** | 需竞争/成本感知的动态分配（异构 cost/load） | announce-bid-award 多轮延迟；bid 评估成瓶颈 |

**对 eli 的决策（修订重点：先把"eli 实际是什么拓扑"说清）：**

- **eli 落地形态的最贴切心智模型是"durable job-queue + serial worker"，不是"多 agent"。** taskboard（`taskboard/mod.rs`）同时是：拓扑（job-board）、协调基质（claim + `Status`）、durability 层（持久 `Status::Blocked{waiting_on}`/`Failed{...}` + `session_origin`）。这三重身份恰好就是 Sidekiq/Celery/Temporal-lite 那一类"持久队列 + 串行 worker"模式。**先把这个模式认下来**，比争论"星形 vs 板"更有用——eli 的下一步不是"多 agent 编排"，是"把一个可恢复的任务队列接通结果回流"。
- **同步/前台子任务默认 orchestrator-worker（星形，through-the-parent）。** parent LLM 调 `agent` tool，workers 互不通信。星形给**单一可审计 vantage**，契合已落地的 per-turn observability（commit `0e7579b`）。
- **拒绝 P2P mesh。** O(n²) + 最弱可观测性，与 eli 的 infinite-context 单 tape 审计哲学冲突。除非将来跨组织，否则不碰。
- **"blackboard"对 eli 而言**：进程内单一真相源是 tape；**跨进程**唯一能当黑板的是**文件系统 / git worktree**（§2c 详述）。不要新建一块抽象板。
- **pub-sub 是演进路径而非起点。** 仅当 agent 数量真正超过"几个"、需要按 role 订阅时引入；即便引入，**tape 仍是 canonical 审计日志**，bus 只搬运瞬态事件。

### 2(b) 消息信封（Envelope Schema）

把 FIPA-ACL（历史正典）+ A2A（当代主流）+ MCP（tool 层）蒸馏出"一个健壮的 inter-agent 信封可能携带什么"。FIPA 的永恒教训：**唯一严格必填的是 intent/performative**；其余（participants、correlation IDs、typed content、interpretation metadata）是反复重新发现的有用字段，但**A2A 在 FIPA 之上新增了 Task/Artifact 对象模型，并非纯改名**。

> ⚠️ 关键澄清（修订）：下表"eli 现状"列**严格区分**两套东西——in-process 通用 envelope（`ValueExt`，无类型 `serde_json::Value`）vs 类型化 sidecar 契约（`SidecarChannelMessage`/`SidecarContext`）。上一稿把二者混淆，高估了 envelope 已携带的字段。

| 关注点 | 字段 | 谁这么干 | eli 现状（已分清两套） |
|---|---|---|---|
| 发送者 | `role` + `sender_id`/`sender_name` | A2A / FIPA | in-process：靠 `context` 约定；sidecar：类型字段 |
| 路由/收件 | `channel`/`channel_target` | A2A url / FIPA receiver | ✅ 约定字段（in-process）/ sidecar 有 |
| **会话分组** | `context_id`（=conversation） | A2A contextId / FIPA conversation-id | ⚠️ 用 `session_id` 兼任 |
| **工作单元** | `task_id` + `state` enum | A2A task+TaskState / ACP run | ❌ in-process 缺（session 混淆了）；taskboard 内部有 task 概念但不在 envelope 上 |
| 消息身份 | `message_id` | A2A / FIPA reply-with | ❌ in-process 无 |
| 关联/线程 | `reply_to_id`/`reference_task_ids` | A2A / FIPA in-reply-to | ⚠️ **仅** sidecar 契约有 `reply_to_id`（`sidecar_contract.rs:39`）+ webhook 松散约定；**in-process envelope 没有** |
| **意图** | `intent`/`performative` | FIPA（唯一必填字段） | ❌ 靠 channel/content 推断 |
| **typed 内容** | `parts[]` with `kind`/MIME | A2A Part.kind / ACP MIME | ⚠️ 扁平 content + 并行 media[] |
| 终态产物 | `artifacts[]`（不可变） | A2A artifacts | ⚠️ 跨进程子 agent 有 `AgentResult.artifacts`（git diff 串），但非 envelope 字段、非结构化 |
| **生命周期** | `state`：含 pause 态(`input-required`/`auth-required`) | A2A TaskState | ⚠️ **仅** sidecar 有 `kind`+`is_active` 布尔（`sidecar_contract.rs:63-65`）；in-process 无生命周期模型；taskboard 有 `Status` enum |
| 流式/追加 | `final`/`append`/`last_chunk` | A2A update events | ❌ |
| 可扩展 | `metadata`/`extensions[]` | A2A / ANP @context | ✅ sidecar `SidecarContext` 用 `#[serde(flatten)]` extra map（`sidecar_contract.rs:50`） |

**对 eli 的决策（修订：只保留便宜可逆的两项，其余降级远期）：**

- **【做，additive】加 `task_id`。** 当前 `session_id` 把"对话"和"工作单元"混为一谈——这是后面所有协调/correlation 问题的根因。`task_id` 是 taskboard task 与未来可恢复任务段的天然关联 key。additive：旧 tape/旧消费者不受影响。
- **【做，便宜可逆，高信号】加显式 `intent` 字段**（`request | inform | delegate | result | critique | cancel`）。FIPA 唯一必填字段的现代再发现；让 hook pipeline（`build_prompt`/`run_model`）确定性路由，而非每次重新推断。**单这一项就值回票价。**
- **【降级，远期 evidence-gated】content 收敛为 typed `parts[]`、`state` enum 取代布尔、`message_id`、`artifacts[]` 对象模型。** 这些是把 A2A 对象模型整体嫁接到一个"信封 = `serde_json::Value`、last-registered-wins、grow-from-hooks"的代码库上——**这是本设计里最大的一处过度工程**。它会触及每个 hook、每个 channel 和持久化的 tape 格式，**只为一个今天只跑一个 LLM 发 tool call、零 live agent-to-agent 流量的系统**。除非真出现并发多 agent 群体，否则不上。
  - 迁移成本提示（上一稿缺失）：一旦把 `intent`/`task_id` 设为"所有 in-process envelope 必填"，**每个现存 builtin 都在产出无类型 Value envelope**（last-registered-wins），全都得改成 stamp 这些字段——会触及每个 hook 和 channel。**所以这两个字段也必须是"可选 + 有默认"，不是 required**，否则迁移故事不成立。

### 2(c) 上下文与记忆共享模型（Context & Memory）——本轴是 eli 的真正发力点，但发力方向修正为 git

| 策略 | 何时用 | 代价 |
|---|---|---|
| **隔离 + 摘要回传**（Anthropic orchestrator-worker） | read-heavy、可分离、输出无需互相 coherent（研究/多源收集） | ~15x token（注：特指 breadth-first 研究负载）；摘要 lossy（telephone game）；写任务会冲突 |
| **单线程 + 压缩模型**（Cognition） | write-heavy、强耦合、产出一个 coherent artifact（编码） | 无并行；压缩"hard to get right"；>100k token 仍 rot |
| **Artifact/file 引用（pointer 非 copy）** | 任何需无损保留细节但不必全驻留的 handoff | 需共享 durable store + 寻址；stale-reference 风险 |
| **Git worktree + diff/commit 串（eli 跨进程现实）** | **CLI 子 agent（主路径）改动代码后回传产物** | 仅覆盖文件改动，不覆盖"推理/决策"；多 worktree 改重叠文件会 merge 冲突 |
| **RAG over 历史** | 历史太大/太旧无法驻留、访问 query-driven | 检索会漏/错排；须抽离散事实而非倒整段 transcript |
| **Schema 化 handoff payload** | 稳定角色、重复 handoff、可靠性>表达自由 | schema 刚性丢 nuance；前期设计成本 |

**对 eli 的决策（修订核心：从"tape-as-blackboard"切换到"git-as-blackboard + tape 仅进程内"）：**

- **跨进程子 agent 的产物通道是 git worktree + diff/commit 串，不是 tape。** 这是 eli **唯一已经在工作**、且**唯一能穿越子进程边界存活**的"共享黑板"：子 agent 在 worktree（`subagent/worktree.rs`）里干活，`collect_artifacts`（`tools.rs:163`）跑 `git log pre..cur` + diff stat 把改动序列化回传，存进 `AgentResult.artifacts`（`tracker.rs:34`）。**这正是"pointer 而非 copy 的无损 handoff"在 eli 已落地的形态**——文件系统是源，diff 串是 pointer/索引。
- **tape 作为隔离/上下文管理仅在进程内有意义。** `fork_tape` 给一个**同进程**子运行干净 window（context quarantine），它**不能**充当 parent↔跨进程 child 的共享黑板（§0 更正 1/2）。所以：
  - **read / 独立任务（研究、多源收集）**：子 agent 输出 → **结构化摘要**回传（接受 Anthropic 的 lossy 摘要 tradeoff），不试图回传 tape 切片。
  - **write / 耦合任务（改代码）**：子 agent 在 worktree 干活 → **git diff/commit 串**回传（无损产物），parent 决定是否 merge worktree。
- **2000-char tail 必须降级为 preview，不是唯一副本。** `format_subagent_completion`（`tools.rs:240`，`SUBAGENT_OUTPUT_TAIL=2000`，`tools.rs:47`）把 tail-truncated stdout 注入 parent context——这是 telephone-game + provenance-collapse 路径。**修复 = tail 降为 *preview*，与 `AgentResult.artifacts`（git 串）并列呈现**，让 parent 看到"改了哪些文件/提交"而非一段被截断的 stdout 尾巴。
- **成本/延迟提醒（上一稿缺失的量化）**：上一稿把"parent 持 pointer、按需 `tape.search`"当作严格更优——但 `tape.search` 是对 JSONL 的 `fetch_all` 全量扫描（`FileTapeStore`）。把一个 2000-char 内联 blob 换成 N 次 `tape.search` 往返，是**用 token 成本换延迟 + I/O + 额外 LLM tool-call turn**。**对很多小结果，2000-char tail 本身更便宜也够用**——所以"pointer 优于 copy"不是无条件的：**默认仍给结构化摘要（小、够用），仅当结果确实大且 parent 确实要追细节时才指向 git 产物**。

### 2(d) 协调与控制（Coordination）

| 机制 | 何时用 | 代价 |
|---|---|---|
| **Handoff-as-tool**（Swarm/Agents SDK/LangGraph Command） | 动态 LLM 路由、triage、不预知下一个 agent | 路由质量赖 LLM judgment；接收方缺"前面做了啥"；handoff 环 |
| **Supervisor 路由**（结构化 RoutingDecision） | 需中心控制+可解释、specialist→supervisor→specialist 循环 | supervisor 单点；每轮回 supervisor 增延迟 |
| **Taskboard claim + queue**（原子 claim、lease、heartbeat、backpressure） | 持久、可观测、串行/有限并行 worker、需 backpressure | 板是争用点；需 claim 原子性+stale 检测 |
| **Contract-net 竞标** | 异构 cost/load 的竞争性分配 | 多轮 announce-bid-award 延迟；bid 评估 |
| **Termination criteria**（机器可检 done-condition） | 永远必须有 | 缺则 step-repetition / 永不停（MAST FM-1.5/3.1，方向性证据） |

**对 eli 的决策：**

- **协调走 taskboard ledger（状态共享），不走直接 agent-to-agent 消息。** eli 已有 `taskboard/mod.rs`：原子 claim、heartbeat、`Status::Blocked{waiting_on}` 依赖边、`session_origin` 溯源。agents 通过读写共享 task **状态**协调，不互发消息。这避免了 mesh 的 O(n²)，也避免 grounding 点名的"单一 global `INBOUND_INJECTOR` slot last-write-wins clobber"问题。
- **delegation 分类的落点是 orchestrator LLM 的推理，不是 `smart_router`（修订更正）。** 上一稿把"read-vs-write delegation 分类"放进 `smart_router` 是范畴错误：`smart_router`（`smart_router.rs:17-22`）是一个**pre-LLM 的问候分类器，只有一个 `Greet(String)` variant**，对任务形状、读写、耦合度一无所知。read/write 的判断**只能发生在 orchestrator LLM 的推理里**（它在 delegate 时自己决定 fan-out vs 单线程），或在 `agent` tool 的参数里由 LLM 显式给出——**不在那个基于规则的预过滤器里**。
- **handoff-as-tool 用作控制转移原语，返回结构化 Command 而非纯文本。** 在 hook pipeline 里建模为一个 tool，产出 `{goto_agent/task_id, context_update, scope}`，在 `render_outbound`/`dispatch_outbound` 消费。保持 LLM 驱动路由、无硬编码边。
- **每次 delegation 必须带显式 termination criteria + effort-scaling 规则**（1 agent 查事实 / 2-4 比较 / 拒绝为强耦合任务 fan-out）。默认单 agent。

---

## 3. 关键张力（Core Tensions）——逐一表态

### 张力 1：隔离 vs 共享上下文（Anthropic vs Cognition）

两篇标题对立的文章其实**没那么对立**。Anthropic 支持并行 subagent，但只用于 **read-heavy、可分离**（研究）；Cognition 反对并行，针对 **write-heavy、强耦合**（编码；Flappy Bird 是其**示意性轶事**：一个 subagent 画 Mario 背景，另一个画不搭的 bird——非测得失败率）。

**eli 立场：把 reconciliation 编码成 delegation policy——"parallelize gathering, serialize deciding"。** 决定变量是**产出是否必须互相 coherent**。read/独立 → fan out（各自隔离 window，回结构化摘要）；write/耦合 → 留单线程，或各自 worktree 干活、由 parent 决定 merge。**这个分类发生在 orchestrator LLM 的 delegate 决策里，不在 `smart_router`。** 默认单 agent——与 eli 的 infinite-context 单 tape 设计和 eval-driven 姿态一致；多 agent 须 evidence-gated。

### 张力 2：信息丢失 vs 上下文污染

- **信息丢失**：每个 summary 步骤 lossy，且 lossy 步骤**乘法式**复合（`A→summary→B→summary→C`）。eli 当前的 2000-char tail 是最坏形态。
- **上下文污染**（Drew Breunig 分类）：context poisoning / distraction（>100k token 退化）/ confusion / clash。

**eli 立场（修订：去掉不可建的"tape 仲裁层"，承认污染在当前架构结构上不会发生）：**
- 反丢失：用 **git diff/commit 串 + 结构化摘要**保留无损产物（源是 worktree 文件系统，diff 串是索引），而不是用一个不存在的"forked tape pointer"。
- 反污染：**eli 当前根本没有"并发共享上下文"路径**——每个 subagent 完全隔离（独立进程 / 独立 in-memory store，§0 更正 1/2）。所以 context poisoning/clash 在当前架构**结构上不可能发生**。
  - ⚠️ **撤回上一稿的"provenance + merge 仲裁层"**：给每条跨边界 tape entry 标 `origin_agent_id`+`confidence`、再加一个"parent 仲裁哪些 entry merge"的协议，是为一个**不存在的并发共享上下文路径**发明一套分布式 quarantine/共识层——**防的是当前架构结构上做不出来的失败模式**，代价是一个没有消费者的仲裁子系统。**删掉。** 若将来真引入并发共享上下文，再回来设计。
- **唯一现实的"写冲突"是 git 层面的**：两个并发子 agent 在各自 worktree 改了重叠文件，parent 要 merge 时 **git 会冲突**（见 §6 失败模式）。这不是"context poisoning",是普通 merge conflict，用 git 既有机制处理。

### 张力 3：透明 vs 噪音

研究方向性发现：**暴露 AI reasoning 增加信任和同意，但可诱发 OVER-trust**，挤出人类独有知识的运用。reasoning 是**说服性启发**，不全是校准辅助。

**eli 立场：collapsed-by-default + 按需展开（layered disclosure）。** 默认渲染 = plan + progress + synthesized result；inter-agent chatter 和 raw tool call 藏在 "expand/replay" 后面。**surface 决策点和 confidence，不是 raw token 流。** 同时配一个按需的 full-trace/replay view（eli 的 append-only tape 天然是 replay 数据源）。注意：这是 UX 原则，不等于现在就建完整 replay viewer（见 §5 trim）。

### 张力 4：成本 vs 覆盖

Anthropic（**特指 breadth-first 研究负载**，非通用常量）：single-agent ≈ 4x chat token；multi-agent ≈ **15x**；该负载下 token 用量解释约 80% 性能方差。

**eli 立场：多 agent 路径默认 cost-gated。** 把 per-subagent / per-handoff token accounting 接到 per-turn observability（commit `0e7599b`/`0e7589b`）。hard per-agent + per-workflow token budget + circuit breaker（HALT-and-surface，不静默 retry）。effort-scaling 规则防过度 spawn。**注意**：硬预算 + 依赖执行器的交互有 liveness 风险（见 §6 新增失败模式），需配 dead-letter，不能让 worker 静默 halt 而 parent 永久等。

---

## 4. eli 落地建议（eli-specific Proposal）

> 原则：**build ON 现有 primitive，不另起炉灶。** 已有 envelope（`serde_json::Value` + `ValueExt`）、tape（进程内 append-only 真相）、hooks（12 点 pipeline）、taskboard（持久 job-board）、subagent（CLI 子进程 + in-process fallback）、git worktree（跨进程产物通道）、sidecar contract、control_plane。下面只组装这些，不发明新抽象。

### 4.1 总体数据流（目标态，已按 §0 更正重画）

```
              parent turn (orchestrator LLM)
                     │  calls `agent` tool with TYPED brief (intent=delegate, task_id)
                     ▼
        ┌────────────────────────────────────────┐
        │ Inter-Agent Brief (envelope)                  │
        │  task_id / intent=delegate                    │
        │  objective, output_format, boundaries, tools[]│
        └────────────────────────────────────────┘
                     │
          ┌──────────┴───────────┐
          ▼                      ▼
   CLI subagent A           CLI subagent B          ← 独立进程 = 天然隔离（非 tape fork）
   (own worktree A)         (own worktree B)
          │ 改代码/提交           │ 改代码/提交
          ▼                      ▼
   collect_artifacts → git    collect_artifacts → git
   diff/commit 串 (无损)       diff/commit 串 (无损)
          │ + 结构化摘要(小)      │ + 结构化摘要(小)
          └──────────┬───────────┘
                     ▼
        ┌────────────────────────────────────────┐
        │ Inter-Agent Result (envelope, intent=result) │
        │  task_id, state(Status enum),                 │
        │  artifacts: <git diff/commit 串> (主产物指针), │
        │  summary{decisions[],files_touched[],         │
        │   verify_status, open_todos[]},               │
        │  preview: <2000-char tail, 降级为预览>          │
        └────────────────────────────────────────┘
                     │  ⚠️ 关键缺口：必须有人把 freeform stdout 解析成上面的 summary
                     ▼
        TaskEvent::Completed ──► control_plane::inject_inbound
                     │  (按 task_id 关联；但见 §6 lifecycle 失败模式)
                     ▼
              parent 读 git 产物串 + 摘要，决定是否 merge worktree
                     │
                     ▼  taskboard: Status Done/Failed, 结果可查
              [optional] dedicated verify pass (Reflexion / diff-as-gate)
```

### 4.2 Inter-agent 消息信封应该装什么 —— 外加"谁来解析 stdout"这个 load-bearing 缺口

复用 `ValueExt`（`envelope.rs:9`）作为访问层，把无类型 Value 约束成一个**文档化的、可选字段**的形状（不是 required，见 §2b 迁移成本）。两个信封：

**Brief（parent → subagent，`intent: "delegate"`）** — 对应 Anthropic 的 task-spec checklist（治 duplicated-work 的修复）：
```
{ task_id, intent: "delegate",
  objective, output_format, boundaries, tools[],
  budget: { max_tokens, max_tool_calls } }
```

**Result（subagent → parent，`intent: "result"`）** — git 产物串为主，摘要为辅，tail 降为 preview：
```
{ task_id, reply_to_id, intent: "result",
  state: <taskboard Status enum>,         // Done | Failed{...} | Blocked{...}
  artifacts: <git diff/commit 串>,         // 主产物：collect_artifacts 已产出
  summary: { decisions[], files_touched[], verify_status, open_todos[] },
  preview: <2000-char tail>,               // 降级：不再是唯一副本
  tokens, duration_ms }
```

> ⚠️ **这是整个 §4.2 的 load-bearing 缺口（上一稿完全没答）：谁把外部 CLI 的 freeform stdout 解析成上面的 `summary{}`？** CLI 子 agent 是一个不透明外部进程（`claude`/`codex`/`kimi`），吐 ANSI 码、进度 spinner、交错 stderr、自然语言散文。三个候选都有硬伤：
> 1. **让子 agent 自己吐 JSON**（prompt `output_format`）：脆弱——CLI 常忽略指令、包裹输出、加 ANSI、拒绝。
> 2. **parent LLM 再摘要**：重新引入它声称要修的 telephone game。
> 3. **确定性 parser 抽取**：从自由散文里抽 `decisions[]` 几乎不可能。
>
> **本稿的决策（务实、最小）**：
> - **`files_touched` / `verify_status` 不靠解析 stdout，而靠 git**：`collect_artifacts` 的 diff/commit 串**确定性**给出改了哪些文件；verify 状态用 diff-as-acceptance-gate（跑测试/编译）确定性判定。这是唯一可靠的来源。
> - **`decisions[]` / `open_todos[]` 是 best-effort**：要求子 agent 输出，缺失时**显式标 `summary_parse_status: "partial|missing"`**，绝不假装抽到了。
> - **永不让"解析失败"= 静默丢工作**：见 §6 新增失败模式"structured summary 抽取失败"。

把 `tape.handoff`（`tools.rs` 内）已手写的 lossy-compression 优先级（"Architecture decisions NEVER summarize … tool outputs keep pass/fail only"）**提升为这个 `summary` 子 schema 的字段**——让 never-summarize 字段结构上 un-droppable。但要清醒：这只对**能可靠填充的字段**（files/verify，来自 git）成立；纯散文决策仍是 best-effort。

### 4.3 subagent 如何交付产物（修订：git 为主，tape fork 仅进程内隔离）

| 任务形状 | 产物通道 | 机制 | 依据 |
|---|---|---|---|
| read / 独立（研究、多源收集） | **结构化摘要回传** | CLI 子进程 stdout → 摘要；无 tape 共享 | Anthropic：隔离 window，回 summary（接受 lossy） |
| write / 耦合（编辑 artifact） | **git worktree + diff/commit 串** | `worktree::create_worktree` + `collect_artifacts`（`tools.rs:163`） | Cognition：implicit decision 体现在 diff；源无损 |
| 同进程 in-process fallback | **ephemeral 隔离 tape（不回 parent）** | `run_in_process` 自建 `InMemoryTapeStore`（`fallback.rs:31`） | 仅隔离，不共享——这是事实，非设计选择 |

> ⚠️ **撤回上一稿的"`fork_tape` 是 eli 的不公平优势"**：`fork_tape`（`store.rs:119`）是**同 task、`task_local`、闭包作用域、退出即丢弃**的上下文隔离 scope，**不产生可寻址 side-tape，不跨进程**。它对 parent↔child 产物交换**无能为力**。真正的"不公平优势"是 **git worktree + `collect_artifacts` 已经在工作**——那才是穿越子进程边界的无损产物通道。

### 4.4 taskboard 如何中介协调 —— 这是最高杠杆的真实 win

taskboard（`taskboard/mod.rs`）今天 **dormant**：`init_task_store` 启动但 `TaskWorker`（`worker.rs:26`）从不在生产 spawn。它已有的能力远超用途：原子 claim、`Status::Blocked{waiting_on}` 依赖边、`Status::Failed{error, stage, tool_trace, retries, suggested_fix}`、`session_origin`、`TaskEvent` broadcast、`is_terminal()`。**它就是一个写好了但没接电的持久 job-queue。**

**决策：把 taskboard 当持久协调 ledger，并补最大缺口——结果回流。**

grounding 点名的最大 gap：Phase-1 worker 只 `store.complete/fail` 写 SQLite，**不调 `inject_inbound`**，所以完成的 task 结果永不自动流回 `session_origin` 对话。而 async `agent.*` 路径**会** `inject_inbound`。两个并行系统不共享状态、不共享结果投递路径、不共享 agent identity。

- **【做，最高杠杆】统一结果投递**：把 `TaskEvent::Completed` 桥接到 `control_plane::inject_inbound`（`control_plane.rs:202`），用 `task_id` 关联。
- **【做，最小】激活一个串行 worker**：claim → execute → report，验证"多 step agent 是否真有需求"。**这是整个路线的 evidence gate。**
- **【改，不是杀】concurrency cap → 软排队**：今天到 `ELI_MAX_CONCURRENT_AGENTS`（默认 5，`tracker.rs:11`）`register` 返回 `false`**婉拒 spawn**（不杀进程，§0 更正 3）。改为让 taskboard 吸收 overflow（backpressure 队列），把"婉拒"变"排队"。
- **【降级，evidence-gated】激活 `Blocked{waiting_on}` 依赖 DAG executor**：⚠️ 这是把 taskboard 变成一个**持久分布式作业调度器**。grounding 明确：worker 已建、已测、**故意不在生产 spawn**；roadmap 100% 是 single-agent hardening。**在没有任何 multi-step agent 需求证据前就上依赖 DAG executor，恰恰违反本文 §7 自己的 evidence-gate。** 故：**先只上"无依赖的串行 worker + inject_inbound 回流"**，依赖排序留到 Phase 1 跑出真实多步需求后再说。

### 4.5 sidecar contract 如何向 A2A-like 演进（修订：整体降级为远期/speculative）

`sidecar_contract.rs` 是 eli **唯一 schema-validated 的跨进程消息格式**（`SIDECAR_CONTRACT_VERSION = "eli.sidecar.v1"`，`sidecar_contract.rs:7`；golden fixtures 在 `sidecar/contracts/v1/`；`SidecarContext` 用 `#[serde(flatten)]` extra map）。它今天只覆盖 **channel traffic（agent↔人/外部）**，不覆盖 agent↔agent。

> ⚠️ **整节降级为 speculative（trim 重点）**：把 sidecar bump v2、写 AgentCard、上 SSE/push notification，是把 eli 定位成"A2A 可互操作网络节点"。**目前没有任何外部 A2A peer 要和 eli 互操作。** 这是为一个纯假设用例建一套**公共协议表面（不可逆、fixture-locked）**。**不做，除非出现真实外部 A2A 对端。**

若将来真要做，保留以下原则（仅作备忘，非 roadmap）：
- 复用契约纪律：typed struct + 共享 golden fixtures + `contract_version`。
- 字段映射：`session_id` → A2A `contextId`；`reply_to_id` → `referenceTaskIds`；`media[]` → typed `parts[]`；`kind`+`is_active` → A2A `TaskState`。
- AgentCard 规范路径是 **`/.well-known/agent.json`**（§0 更正 6；上一稿写错为 `agent-card.json`）。
- 保持两契约不合一：tools = MCP（agent↔tool，纵向）；sidecar/gateway = A2A surface（agent↔agent，横向）。

### 4.6 隔离策略如何保住 infinite-context 不变式

infinite-context 不变式：**tape 是无界真相源，context window 是 lossy view，优化不能破坏这一点**（project memory）。

- **进程内**：tape 仍是该进程的 append-only 真相；`fork_tape` 给同进程子运行一个干净切片，退出即并入或丢弃——**这只在单进程内有意义**。
- **跨进程**：子 agent 的真相源是**它自己 worktree 的文件系统 + git 历史**；parent 拿到的是 git diff/commit 串（无损索引）+ 结构化摘要。**隔离不丢信息**——源（worktree/git）始终权威，parent 按需读 git 而非把全 transcript 塞进窗口，避免 distraction/confusion。
- intra-session auto-handoff（`agent_run.rs`：`should_handoff`/`place_handoff_anchor`，`ELI_HANDOFF_THRESHOLD_PCT` 默认 40）与 inter-agent 投递是**两个不同机制**——文档必须明确区分，停止用 "handoff" 一词污染两者。

### 4.7 ⚠️ 不可逆决策（需用户显式 sign-off）

以下一旦发布就难回退（公共 API / 序列化格式 / 契约 schema），按 Phase-2 规则**必须停下等确认**：

1. **Envelope 加 `task_id` / `intent` 字段。** 改变 in-process 消息形状 + tape 中持久化 envelope。**强制做成 additive + 可选 + 有默认**（旧 tape 仍可读，现存 builtin 不必全改），降到可逆。typed `parts[]` / `state` enum / `message_id` **不在 Phase 1/2 范围**（§2b 降级）。
2. **subagent result contract 从 "truncated stdout string" 改 "git artifacts 串 + 结构化摘要 + preview"。** 改变 parent LLM 看到的东西。做成 additive（tail 仍在，作 preview）。
3. **TaskWorker 是否成为真正的 multi-step executor。** 头号 open question。**建议：先激活最小 serial worker + inject_inbound 回流验证需求**，再决定是否上依赖 DAG/worker pool——evidence-gated。
4. **（远期，默认不做）sidecar contract 升 v2 + AgentCard。** 仅当出现真实外部 A2A 对端才考虑；届时同步 `sidecar/contracts/v2/` golden fixtures。

---

## 5. 产品/UX 设计（Product Surface）—— 大幅 trim，只留 Phase 1 真正需要的

> 修订：上一稿的 §5 是一套**完整的 agent 编排 IDE UX**（plan-gate + live progress tree + interrupt/redirect + replay/time-travel + attribution 分层 + 树内 per-agent 成本）。对一个目前只呈现单一连贯 "eli" actor 的 CLI/Telegram 工具，建一个非阻塞 TUI 进度树 + time-travel viewer 是**由一个 Phase 1 尚未证明有人要的多 agent fan-out 来 justify 的大前端投入**。下面**只保留 collapsed-by-default 这一个 Phase 1 必需项**，其余降级。

```
┌─ eli UX（Phase 1 范围）──────────────────────────────────────┐
│  ① COLLAPSED 默认   显示 synthesized result；raw tool/chatter │
│       藏 "expand" 后（trust-calibration：少诱发 over-trust） │
│  ② REPLAY（near-free）  append-only tape 已是数据源，缺一个 view│
└──────────────────────────────────────────────────────────────┘

┌─ 远期 / evidence-gated（仅当多 agent fan-out 被证明有人要）──┐
│  ③ PLAN-GATE hook     执行前给可编辑 plan（有副作用时）        │
│  ④ LIVE PROGRESS TREE  per-agent 状态 + cost（必须 non-block） │
│  ⑤ INTERRUPT/REDIRECT  pause + 条件断点 + chat 重定向          │
│  ⑥ ATTRIBUTION 分层    对外单一 "eli"；调试视图命名+树         │
└──────────────────────────────────────────────────────────────┘
```

| 原语（Phase 1） | 在 eli 怎么落 | 渠道差异 |
|---|---|---|
| **Collapsed + layered disclosure** | 默认渲染 synthesized result；`expand` 命令拉 tape replay | 两渠道默认折叠 |
| **Replay** | tape 已 append-only/anchorable/forkable——数据已在，只缺一个只读 view | CLI：`eli tape replay <session>` |

远期原语（③④⑤⑥）保留设计意图记录，但**不在 Phase 1/2 实施**，理由见上。其中 ④ progress tree **若实施必须 async 非阻塞渲染**（否则 fan-out 冻 TUI）——这条约束先记下，免得将来踩。

---

## 6. 失败模式与防护（Failure Modes & Guardrails）

> MAST 分类（UC Berkeley）方向性提示：失败大多是系统设计/协调问题，不是 base-model 能力；multi-level verification 是较高 ROI 的干预。**注意：MAST 的具体百分比/Kappa/delta 是跨异构开源框架的聚合，方向性参考，非 eli 的保证收益。**
>
> 修订：上一稿在 §6 一次性推荐了**六套 guardrail 子系统**（hash response ledger、per-tool caps、output-similarity stall 检测、cycle 检测、独立 verify、budget circuit breaker、provenance），其中多数针对**当前单 agent 隔离子进程拓扑根本不会发生**的 multi-agent loop/poisoning。下表**只保留当前架构真实暴露的失败模式**，并**新增 5 个上一稿漏掉的、本架构必然出现的失败模式**。

| 失败模式 | 当前架构真会发生吗 | eli 防护 | 复用 |
|---|---|---|---|
| **Telephone-game / 截断丢失** | ✅ 现在就在发生 | 2000-char tail 降为 preview；git diff/commit 串作主产物指针；结构化摘要 | `collect_artifacts`（已落地） |
| **结果不回流对话** | ✅ 现在就在发生（worker 不调 inject_inbound） | `TaskEvent::Completed → inject_inbound`，按 task_id 关联 | taskboard + control_plane |
| **spawn 被婉拒（到并发上限）** | ✅（软拒绝，非崩溃） | cap → backpressure 队列，把"拒绝"变"排队" | `AgentTracker` + taskboard |
| **缺终止条件 / 早停** | ✅（单 agent 也会 loop） | 每 delegation 带机器可检 done-condition；termination 前 verify gate | — |
| **验证缺失 / reasoning-action mismatch** | ✅ | 独立 verify pass（不让生产 agent 自验）；代码类用 diff-as-acceptance-gate（跑测试/编译） | **Reflexion verify/self-correct loop（commit `b83ee7e`）就是 evaluator-optimizer 原语** |
| **未澄清就动手** | ✅ | plan-gate（有副作用时）+ 显式"需输入"态 | — |
| **Loop / ping-pong 烧 token** | ⚠️ 单 agent 内可能 | per-tool call caps + retry 上限 + 指数退避 + dead-letter | channel dispatch retry + dead-letter（已落地） |
| **Cost/token 爆炸** | ✅（尤其多 step） | hard per-agent + per-workflow budget + circuit breaker（HALT-and-surface）+ per-agent cost attribution | per-turn observability（`0e7589b`）；`BudgetLedger`（`control_plane.rs`） |
| ~~并发写 context 污染 / poisoning~~ | ❌ **结构上不可能**（每子 agent 完全隔离） | **不建仲裁/provenance 层**（§3 张力2 撤回） | — |

### 6.1 新增失败模式（上一稿漏掉，本架构必然出现）

1. **【最高频真实失败】子进程 stdout 解析失败 → 结构化摘要抽不出。** 整个 typed-Result 依赖从外部 CLI stdout 抽 `decisions[]/files_touched[]/verify_status`，但 CLI 吐 ANSI/spinner/交错 stderr/散文，**经常无视 `output_format` 指令**。
   **防护**：`files_touched`/`verify_status` **不靠解析 stdout，靠 git**（`collect_artifacts` 确定性给文件清单；测试/编译确定性给 verify）。散文型 `decisions[]` 缺失时显式标 `summary_parse_status: partial|missing`，**绝不静默丢弃子 agent 的工作**——把原始 stdout 作为 preview 保留 + git 产物串保留，让 parent 仍能从 git 看到实际改动。

2. **悬空指针（stale-reference）—— 这是 pointer 模型在 eli 上的主导失败。** 上一稿的 `tape_ref` 必然悬空，因为 fork store 不持久（§0 更正 1）。
   **防护**：**本稿不用 tape_ref**。指针指向 **git（worktree 路径 + commit 范围）**——只要 worktree 未清理就可解析。worktree 已清理（无改动时 auto-remove，`tools.rs:2417`）时，diff/commit 串本身（已捕获在 `AgentResult.artifacts`）即自包含快照，**不依赖任何活体引用**。

3. **`inject_inbound` correlation race —— 没有活体 turn 可关联进去（lifecycle 不可能性）。** 上一稿提议 `TaskEvent::Completed → inject_inbound` 按 `task_id` 关联回"in-flight 的发起 turn"。但：(a) 全局只有一个 `INBOUND_INJECTOR` slot（last-write-wins，`control_plane.rs:187`）；(b) turn 是 per-inbound tokio task，**完成时早已返回**——**没有活体 turn 可以关联进去**，结果只能**启动一个新 turn**。
   **防护**：诚实接受"结果回流 = 启动一个携带 `task_id`/`session_origin` 的**新 turn**（而非注入旧 turn）"。新 turn 的 prompt 里带上 `task_id` + 原始 objective + git 产物串，让新 turn 在原会话上下文里**续接**而非"假装注入了一个已死的 turn"。这是 §4.4 的真实形态。

4. **Worktree merge / 冲突失败（"parallelize 后 reconcile"在 write 任务上的现实结局）。** 两个并发子 agent 在各自 worktree 改重叠文件，parent merge 时 **git 会冲突**。
   ⚠️ **同时修正上一稿的自相矛盾**：上一稿一边说"私有 fork 写面分离让冲突结构上不可能",一边在 §6 承认 worktree"无 reconcile 协议"——**这两句矛盾**：文件系统 worktree **确实会**在 merge 时冲突。
   **防护**：写任务**默认串行**（"serialize deciding"），不并发改重叠文件；若确需并发，merge 走标准 git 三方合并，冲突时**上报给 parent/人**（不自动猜），并把冲突 hunk 作为结构化 `open_todos` 返回。**不假装冲突不存在。**

5. **预算 circuit-breaker 误触 / 死锁（liveness）。** 硬 per-agent/per-workflow 预算 + HALT-and-surface：一个合法需超预算的 workflow 会**死锁**——worker halt，parent 等一个永不来的结果，若上了 `Blocked{waiting_on}` 依赖图则那条边**永不解析**。
   **防护**：预算 halt **必须**写入 taskboard 为 `Status::Failed{error: "budget_exceeded", suggested_fix}` 并触发 `inject_inbound`（dead-letter 回流），**绝不让 worker 静默 halt 而 parent 无限等**。依赖图节点见到上游 `Failed` 即向下传播失败，不悬挂。

6. **边界 schema 校验拒绝 → 已烧 token 的工作被搁浅。** 若在 dispatch 前校验 Brief/Result（像 sidecar fixtures），而一个 in-process inter-agent 信封校验**失败**，那个**已经烧了 token 跑完**的子进程工作怎么办？
   **防护**：校验失败**不丢工作**——把原始 `artifacts`（git 串）+ raw stdout 落入 **dead-letter**（taskboard `Failed{stage: "result_validation", tool_trace}`），并回流告知 parent"结果存在但 schema 不合法，原始产物在此"。校验是**软门 + 降级路径**，不是硬丢弃。

**Schema validation at boundaries（保留，但限定范围）**：把 §4.2 的 Brief/Result 在 dispatch 前 validate，**但仅作软门 + dead-letter 降级**（见上第 6 条），不作硬拒。研究的反模式是"agent 间传无结构自由文本"——schema 化减方差、可审计；但在 eli，**最可靠的结构来自 git，不来自解析 LLM 散文**。

---

## 7. 分阶段路线（Phased Roadmap）

> 每阶段尽量可逆；不可逆决策（§4.7）在进入对应阶段前停下等 sign-off。**路线已按 critique 收紧：Phase 1 是真 win，Phase 2 大幅瘦身，Phase 3 默认不做。**

### Phase 0 — 词汇 + 文档对齐（纯可逆，零代码风险）
- 拆开两个 "handoff" 的命名（intra-session compaction vs inter-agent delivery）。
- 在 `AGENTS.md` 文档化：跨进程产物通道 = **git worktree + `collect_artifacts`**，不是 tape；tape 隔离 = **进程内**。明确 `fork_tape` 是上下文隔离 scope、非持久 side-tape。
- **产出**：一致且**与代码相符**的心智模型。无序列化变更。

### Phase 1 — MVP：修 telephone-game + 结果回流（最高杠杆，大半可逆）—— **这是该 ship 的**
- **subagent result：truncated blob → git artifacts 串 + 结构化摘要 + preview**（additive：保留 tail 作 preview，加 `artifacts`/`summary{}`）。`files_touched`/`verify_status` 从 git/测试**确定性**得来，散文字段 best-effort + `summary_parse_status`。
- **激活最小 serial TaskWorker + `TaskEvent::Completed → inject_inbound`**，结果以**带 `task_id` 的新 turn**回流（接受 §6 失败模式 3 的 lifecycle 现实，不假装注入旧 turn）。**这是验证"多 agent 是否真有需求"的 evidence gate。**
- envelope 加 `task_id` + `intent`（**additive、可选、有默认**）。
- concurrency cap → backpressure 队列（把"婉拒 spawn"变"排队"）。
- **UX**：collapsed-by-default + `expand`（tape replay）；per-agent token 接 observability。
- ⚠️ sign-off 点：result contract 形状变更（§4.7 #2）。

### Phase 2 — 结构化协调（仅在 Phase 1 证明需求后；大幅瘦身）
- Brief/Result schema validation at boundaries（**软门 + dead-letter**，§6.1#6）+ golden fixtures。
- **guardrails（只上当前架构真暴露的）**：per-tool caps + retry 上限 + budget circuit breaker（带 dead-letter 防死锁，§6.1#5）+ 独立 verify pass（复用 Reflexion loop / diff-as-gate）。
- **写任务默认串行**；并发 worktree 冲突走标准 git 合并 + 上报（§6.1#4）。
- ⚠️ **明确不做（evidence-gated 推迟）**：typed `parts[]`/`state` enum/`message_id` 全套对象模型、provenance + merge 仲裁层、`Blocked{waiting_on}` 依赖 DAG executor、§5 的 plan-gate/progress-tree/interrupt/replay-viewer。这些是为不存在的并发共享上下文群体准备的。
- ⚠️ sign-off 点：任何 envelope 必填字段变更 + TaskWorker 是否升级为依赖执行器（#3）。

### Phase 3 — A2A 互操作（默认不做，speculative）
- sidecar contract v2 + AgentCard（`/.well-known/agent.json`）+ SSE/push notification——**仅当出现真实外部 A2A 对端**。无对端则**不建这层不可逆公共协议表面**。

### 何时**不要**继续（反向 gate）
多 agent（特指可并行、低耦合、breadth-first 负载）约 15x token——方向性证据。eli roadmap 当前全是 single-agent hardening，**没有 multi-agent 条目**。所以多 agent 扩张应 **evidence-gated**：Phase 1 的最小 serial worker 跑出真实用量证明需求后，再投 Phase 2。若证据不足，**停在 Phase 1**，让 `agent`-tool 子进程仍是唯一执行路径，taskboard 留作（现在已接通结果回流的）passive tracker。这与项目 eval-driven 姿态一致：*"add complexity only when it demonstrably improves outcomes."*——**注意：上一稿提议的依赖 DAG executor 恰好违反了这条自家 gate，本稿已将其推迟。**

---

### 附：一句话决策清单（修订后）

1. **拓扑** → 认下"durable job-queue + serial worker"才是 eli 实际形态；同步子任务 orchestrator-worker 星形；拒 mesh。
2. **信封** → 只加 `task_id` + `intent`（additive、可选、有默认）；A2A 对象模型全套推迟。
3. **上下文** → **跨进程产物 = git worktree + diff/commit 串**（`collect_artifacts`）；tape 隔离仅进程内；2000-char tail 降为 preview；结构化字段优先从 git 取，散文 best-effort。
4. **协调** → taskboard ledger（状态共享非消息）；激活**无依赖的**最小 serial worker + `inject_inbound` 回流（以新 turn 形式）；cap → 队列；依赖 DAG executor 推迟。
5. **UX** → 只上 collapsed-by-default + tape replay；plan-gate/progress-tree/interrupt 推迟到多 agent 被证明有需求。
6. **防护** → 只防当前架构真暴露的（telephone-game、结果不回流、stdout 解析失败、worktree 冲突、预算死锁、校验搁浅）；**不建 provenance/merge 仲裁层**（污染在隔离架构下结构上不可能）。
7. **节奏** → evidence-gated；Phase 1 验证需求，stdout-解析/result-format/task_id 显式 sign-off；A2A 表面默认不做。

**核心洞察（修订）**：eli 不缺多 agent 基建，缺的是**把已经在工作的 git 产物通道当主产物指针、把 dormant 的持久任务队列接通结果回流、把 2000-char tail 降级为 preview**。最大杠杆不是"把 tape 改造成跨 agent 黑板"（那条路在现有 primitive 上不存在——`fork_tape` 是同进程隔离 scope，不跨进程、不持久、无可寻址引用），而是**停止用截断 stdout 当唯一副本，改用 git diff/commit 串这个 eli 早已能跨进程无损交付的真相通道**。

---

### 附：本次修订对照（critique → 处置）

- **核心机制不可建（fork_tape/tape_ref）**：撤回，改 git worktree+diff 通道。（§0.1-0.2, §2c, §4.1, §4.3）
- **in-process fallback 不共享 parent tape**：更正。（§0.2, §4.3）
- **并发 cap 是软拒绝非杀进程**：更正。（§0.3, §4.4, §6）
- **envelope 不含 reply_to_id/kind/is_active**：更正，分清两套契约。（§0.4, §2b）
- **A2A≠FIPA 改名 / AgentCard 路径 / Anthropic·Cognition·MAST 数字带条件**：全部加 caveat。（§0.6, §3, §6）
- **smart_router 是范畴错误**：更正落点到 orchestrator LLM。（§2d, §3 张力1）
- **谁解析 stdout（load-bearing gap）**：新增，靠 git 确定性 + 散文 best-effort + parse_status。（§4.2, §6.1#1）
- **缺失失败模式**：新增 6 条（stdout 解析、悬空指针、inject 无活体 turn、worktree 冲突、预算死锁、校验搁浅）。（§6.1）
- **过度工程 trim**：A2A 对象模型、provenance/仲裁层、依赖 DAG executor、§5 全套 UX、sidecar v2 全部降级 evidence-gated/默认不做。（§2b, §3, §4.4, §4.5, §5, §7）
- **保留并加固**：Phase 0 词汇拆分 + Phase 1 inject_inbound 回流 + task_id/intent additive + tail 降 preview——critique 认定为真 win，本稿置于 Phase 1。

相关文件（绝对路径，供后续实现参考）：
- `/Users/bytedance/code/eli/crates/eli/src/builtin/store.rs` — `ForkTapeStore::fork:119`（同 task `task_local` 隔离 scope，**非持久 side-tape**）
- `/Users/bytedance/code/eli/crates/eli/src/builtin/tools.rs` — `collect_artifacts:163`（**跨进程 git 产物通道**）/ `SUBAGENT_OUTPUT_TAIL:47` / `format_subagent_completion:~240`（telephone-game gap）
- `/Users/bytedance/code/eli/crates/eli/src/builtin/subagent/worktree.rs` — `create_worktree`/`cleanup_worktree`（隔离写面）
- `/Users/bytedance/code/eli/crates/eli/src/builtin/subagent/fallback.rs` — `run_in_process:26`（自建 store，**不共享 parent tape**）
- `/Users/bytedance/code/eli/crates/eli/src/builtin/subagent/tracker.rs` — `AgentResult.artifacts:34` / `register:84`（**软拒绝非杀进程**）/ `ELI_MAX_CONCURRENT_AGENTS:9`
- `/Users/bytedance/code/eli/crates/eli/src/control_plane.rs` — `INBOUND_INJECTOR:187` / `inject_inbound:202` / `BudgetLedger`（结果回流 + 预算）
- `/Users/bytedance/code/eli/crates/eli/src/smart_router.rs` — `RouteDecision::Greet:21`（**仅问候分类，非任务路由落点**）
- `/Users/bytedance/code/eli/crates/eli/src/taskboard/mod.rs` — `Task` / `Status`（含 `Blocked{waiting_on}` / `session_origin` / `is_terminal`）— 持久 job-queue
- `/Users/bytedance/code/eli/crates/eli/src/taskboard/worker.rs` — `TaskWorker:26`（dormant，Phase 1 激活 + inject_inbound）
- `/Users/bytedance/code/eli/crates/eli/src/envelope.rs` — `ValueExt:9`（`field`/`content_text`/`normalize_envelope`/`unpack_batch`——**无 reply_to_id**）
- `/Users/bytedance/code/eli/crates/eli/src/sidecar_contract.rs` — `SIDECAR_CONTRACT_VERSION:7` / `reply_to_id:39` / `is_active:63` / `kind:65`（类型化契约，**勿与 in-process envelope 混淆**）；fixtures 在 `/Users/bytedance/code/eli/sidecar/contracts/v1/`
- `/Users/bytedance/code/eli/crates/eli/src/builtin/agent/agent_run.rs` — `should_handoff` / `place_handoff_anchor`（intra-session，须与 inter-agent 区分）
