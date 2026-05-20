---
name: feishu-bitable
description: "Create, query, edit, and manage Feishu Bitable (multidimensional spreadsheets) via lark-cli base — apps, tables, fields, records, views, dashboards, forms, workflows."
---

# feishu-bitable

Bitable / 多维表格 操作走 `lark-cli base`。

## Prerequisites

- 写记录前先 `+field-list` 拿字段 type/ui_type
- 批量上限 500 / call；并发写同表不支持，串行 + 0.5–1s 间隔
- `+base-create` 自带默认空表 + 默认行 → 写之前先 list+batch_delete 清掉

## Field value formats (must match type)

| type | ui_type | 字段 | 正确格式 | 常见错误 |
|------|---------|------|----------|----------|
| 11 | User | 人员 | `[{"id":"ou_xxx"}]` | 传字符串 `"ou_xxx"` |
| 5 | DateTime | 日期 | `1674206443000` (毫秒) | 用秒 / 字符串 |
| 3 | SingleSelect | 单选 | `"选项名"` | 传数组 `["选项名"]` |
| 4 | MultiSelect | 多选 | `["A","B"]` | 传字符串 |
| 15 | Url | 超链接 | `{"link":"...","text":"..."}` | 传 bare URL |
| 17 | Attachment | 附件 | `[{"file_token":"..."}]` | 传外部 URL |

错误码 `125406X` / `1254015` 通常是格式不匹配。

## Quick Reference

| Intent | Command |
|--------|---------|
| 新建 Base | `lark-cli base +base-create --name "..." --folder-token fldcnXXXX` |
| 拿 Base 信息 | `lark-cli base +base-get --app-token <app_token>` |
| 复制 Base | `lark-cli base +base-copy --app-token <app_token> --name "..."` |
| 建表 | `lark-cli base +table-create --app-token <t> --name "..." --fields-file ./fields.json` |
| 列字段 | `lark-cli base +field-list --app-token <t> --table-id <tid>` |
| 加字段 | `lark-cli base +field-create --app-token <t> --table-id <tid> --field-name "..." --type 1` |
| 删字段 | `lark-cli base +field-delete --app-token <t> --table-id <tid> --field-id <fid>` |
| 列记录 | `lark-cli base +record-list --app-token <t> --table-id <tid>` |
| 加记录 | `lark-cli base +record-create --app-token <t> --table-id <tid> --data '{"字段A":"值"}'` |
| 批量加 (≤500) | `lark-cli base +record-batch-create --app-token <t> --table-id <tid> --records-file ./records.json` |
| 改记录 | `lark-cli base +record-update --app-token <t> --table-id <tid> --record-id <rid> --data '{"字段A":"新值"}'` |
| 批量改 (≤500) | `lark-cli base +record-batch-update --app-token <t> --table-id <tid> --records-file ./updates.json` |
| 批量删 | `lark-cli base +record-batch-delete --app-token <t> --table-id <tid> --record-ids rec_a,rec_b` |
| 聚合/过滤 (DSL) | `lark-cli base +data-query --app-token <t> --table-id <tid> --dsl-file ./query.json` |
| 列视图 | `lark-cli base +view-list --app-token <t> --table-id <tid>` |
| 仪表盘 | `lark-cli base +dashboard-list/create/update/delete --app-token <t>` |
| 表单 | `lark-cli base +form-*` |
| 工作流 | `lark-cli base +workflow-list/create/update` |
| 权限/角色 | `lark-cli base +advperm-enable/disable` + `+role-*` |

## Examples

```bash
# 1) 拿字段定义
lark-cli base +field-list --app-token bascnXXXX --table-id tbl_yyy \
  --jq '.items[]|{field_name,type,ui_type}'

# 2) 批量插记录
cat > /tmp/records.json <<'EOF'
[
  {"fields":{"任务":"修 bug","负责人":[{"id":"ou_xxx"}],"截止":1747900800000}},
  {"fields":{"任务":"写文档","负责人":[{"id":"ou_yyy"}],"截止":1747987200000}}
]
EOF
lark-cli base +record-batch-create --app-token bascnXXXX --table-id tbl_yyy --records-file /tmp/records.json

# 3) 跨表/汇总查询
cat > /tmp/q.json <<'EOF'
{"aggregations":[{"field":"金额","aggregator":"sum"}],
 "filter":{"conditions":[{"field":"状态","operator":"is","value":["已完成"]}]}}
EOF
lark-cli base +data-query --app-token bascnXXXX --table-id tbl_yyy --dsl-file /tmp/q.json
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 不看字段类型直接拼 value | 先 `+field-list` 看 `type`/`ui_type` |
| 一次写 >500 条 | 拆批，每批 ≤ 500 |
| 并发写同表 | 必须串行；并发会冲突 |
| `+base-create` 后立刻插入 | 默认带空表 + 默认行；要先清 |
| 拿 wiki 链接当 app_token | wiki 先 `wiki spaces get_node` 拿 obj_token；bitable 的 obj_token 才是 app_token |
| 人员字段传 `"ou_xxx"` 字符串 | `[{"id":"ou_xxx"}]` |
| 日期字段传 `"2026-05-22"` | 毫秒时间戳 `1747900800000` |
