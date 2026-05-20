---
name: feishu-calendar
description: "Manage Feishu calendar events via lark-cli — create/update/delete events, manage attendees, query free/busy and suggest available time slots, find and book meeting rooms."
---

# feishu-calendar

All operations go through `lark-cli calendar`. Use the `+` shortcuts first — they handle multi-step orchestration; only fall back to raw `calendar events/calendars/freebusys` subcommands when shortcuts don't cover the case.

## Time/ID conventions

- Timezone: Asia/Shanghai (UTC+8). All times must be ISO 8601 with timezone, e.g. `2026-05-21T14:00:00+08:00`.
- IDs: user `ou_...`, group `oc_...`, room `omm_...`, email `name@domain`.
- 编辑既有日程：先定位 `event_id`（用 `+agenda` 或 `events search_event`）。重复性日程要操作的是**实例的 event_id**，绝不能用原始 series id。
- 删除/修改后立即二次查询前等 ≥ 2 秒（同步延迟）。

## Quick Reference

| Intent | Command |
|--------|---------|
| 查看今日/近期日程 | `lark-cli calendar +agenda` |
| 查询某天 | `lark-cli calendar +agenda --date 2026-05-21` |
| 创建日程 | `lark-cli calendar +create --summary "..." --start "2026-05-21T14:00:00+08:00" --end "2026-05-21T15:00:00+08:00"` |
| 创建并邀请 + 订会议室 | `lark-cli calendar +create --summary "..." --start ... --end ... --attendees ou_xxx,name@x.com --rooms omm_xxx` |
| 更新已有日程 | `lark-cli calendar +update --event-id <id> --summary "..."` |
| 增删参与人 | `lark-cli calendar +update --event-id <id> --add-attendees ou_xxx --remove-attendees ou_yyy` |
| 删除日程 | `lark-cli calendar events delete --params '{"calendar_id":"primary","event_id":"<id>"}'` |
| 忙闲查询 | `lark-cli calendar +freebusy --user-ids ou_xxx,ou_yyy --start ... --end ...` |
| 智能找时间 | `lark-cli calendar +suggestion --participants ou_xxx,ou_yyy --duration 30m` |
| 找可用会议室 | `lark-cli calendar +room-find --start ... --end ... --capacity 6` |
| RSVP 回复 | `lark-cli calendar +rsvp --event-id <id> --status accept|decline|tentative` |

## Examples

```bash
# 查询本周日历
lark-cli calendar +agenda --range thisweek

# 创建会议，邀请两人 + 一间会议室
lark-cli calendar +create \
  --summary "周会同步" \
  --start "2026-05-22T10:00:00+08:00" \
  --end   "2026-05-22T11:00:00+08:00" \
  --attendees ou_aaa,ou_bbb \
  --rooms omm_room1

# 给某个重复日程的 5/22 那次加一个人
# 1) 先定位实例的 event_id
lark-cli calendar +agenda --date 2026-05-22 --jq '.items[]|select(.summary=="周会")'
# 2) 再用实例 event_id update
lark-cli calendar +update --event-id <instance_event_id> --add-attendees ou_ccc
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 用 series event_id 改重复日程的某一次 | 先用 `+agenda` 拿到实例 event_id 再操作 |
| 拿口语化的"日历"当 calendar 容器去操作 | 默认意图是 event（日程），用 `+create/+agenda` |
| 预约过去时间 | 禁止；唯一例外是跨越当前时刻的日程 |
| 自己构造 attendees 数组 | `+create/+update` 已接受逗号分隔的混合 id/email |
| 询问过去会议（参与人/纪要） | 走 `feishu-vc` / `lark-cli vc` |
